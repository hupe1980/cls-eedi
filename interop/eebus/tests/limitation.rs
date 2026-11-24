use std::time::Duration;

use cls_eedi::eebus::{Completion, LimitAdapter};
use cls_eedi::engine::{Backend, BackendEvent, Clock, DeviceEvent, LocalDevice};
use cls_eedi::wire::{Control, Limit, Message, Power};
use eebus::model::{
    self, CmdClassifier, Datagram, DeviceType, EntityType, FeatureAddress, FeatureType, Function,
    Role,
};
use eebus::spine::{
    Engine, ErrorNumber, LocalDevice as SpineDevice, LocalEntity, LocalFeature, SpineEvent,
};
use eebus::usecases::{
    limitation::{self, Direction},
    lpc, lpp,
};

struct Equipment {
    device: Engine,
    actor: limitation::ControllableSystemActor,
    client: FeatureAddress,
    server: FeatureAddress,
}

impl Equipment {
    fn bound(direction: Direction, failsafe_watts: f64) -> (Self, Engine) {
        let (name, device_type, entity_type) = match direction {
            Direction::Consumption => (
                "pump",
                DeviceType::HeatGenerationSystem,
                EntityType::HeatPumpAppliance,
            ),
            Direction::Production => ("inverter", DeviceType::Inverter, EntityType::PVSystem),
        };
        let mut device = SpineDevice::new("i:33100", name, device_type).unwrap();
        device
            .add_entity(
                LocalEntity::new([1], entity_type)
                    .with_feature(limitation::load_control_feature(1))
                    .with_feature(limitation::device_configuration_feature(2))
                    .with_feature(limitation::device_diagnosis_feature(3))
                    .with_feature(limitation::device_diagnosis_client_feature(4)),
            )
            .unwrap();
        let features = limitation::CsFeatures {
            load_control: device.address_of(&[1], 1),
            device_configuration: device.address_of(&[1], 2),
            device_diagnosis: device.address_of(&[1], 3),
            device_diagnosis_client: device.address_of(&[1], 4),
        };
        let server = features.load_control.clone();
        let mut device = Engine::new(device);
        let actor = limitation::ControllableSystemActor::builder(
            limitation::ControllableSystem::new(
                limitation::CsConfig::new(failsafe_watts, Duration::from_secs(7200)),
                Duration::ZERO,
            ),
            direction,
            features,
        )
        .install(&mut device, Duration::ZERO);
        let mut guard =
            SpineDevice::new("i:33200", "controller", DeviceType::ElectricitySupplySystem).unwrap();
        guard
            .add_entity(
                LocalEntity::new([1], EntityType::GridGuard).with_feature(LocalFeature::new(
                    1,
                    FeatureType::Generic,
                    Role::Client,
                )),
            )
            .unwrap();
        let client = guard.address_of(&[1], 1);
        let mut guard = Engine::new(guard);
        guard.request_binding(&client, &server, Duration::ZERO);
        let mut pair = Self {
            device,
            actor,
            client,
            server,
        };
        pair.exchange(&mut guard, Duration::ZERO);
        (pair, guard)
    }

    fn exchange(&mut self, guard: &mut Engine, now: Duration) -> Vec<SpineEvent> {
        let mut events = Vec::new();
        for _ in 0..32 {
            let mut work = false;
            while let Some(packet) = guard.poll_transmit() {
                self.device.handle_datagram(&over_wire(&packet), now);
                work = true;
            }
            while let Some(event) = self.device.poll_event() {
                self.actor.handle_event(&mut self.device, &event, now);
                work = true;
            }
            while let Some(packet) = self.device.poll_transmit() {
                guard.handle_datagram(&over_wire(&packet), now);
                work = true;
            }
            while let Some(event) = guard.poll_event() {
                events.push(event);
            }
            if !work {
                return events;
            }
        }
        panic!("SPINE exchange failed to quiesce");
    }

    fn read_limits(&mut self, guard: &mut Engine) {
        for function in [
            Function::LoadControlLimitDescriptionListData,
            Function::LoadControlLimitListData,
        ] {
            let counter = guard.read(&self.server, &self.client, function, Duration::ZERO);
            let request = guard.poll_transmit().unwrap();
            self.device
                .handle_datagram(&over_wire(&request), Duration::ZERO);
            let reply = self.device.poll_transmit().unwrap();
            let header = reply.header.as_ref().unwrap();
            assert_eq!(header.address_source.as_ref(), Some(&self.server));
            assert_eq!(header.address_destination.as_ref(), Some(&self.client));
            assert_eq!(header.msg_counter_reference, Some(counter));
            assert_eq!(header.cmd_classifier, Some(CmdClassifier::Reply));
            guard.handle_datagram(&over_wire(&reply), Duration::ZERO);
        }
    }

    fn apply(
        &mut self,
        guard: Engine,
        device: &mut LocalDevice,
        request_id: &str,
        now: Clock,
    ) -> (Completion, Engine, Datagram) {
        let mut adapter =
            LimitAdapter::new(device, guard, self.client.clone(), self.server.clone()).unwrap();
        let counter = adapter.dispatch(request_id, now).unwrap();
        assert!(adapter.dispatch(request_id, now).is_err());
        let packet = adapter
            .poll_transmit()
            .expect("adapter queued a limit write");
        let header = packet.header.as_ref().unwrap();
        assert_eq!(header.cmd_classifier, Some(CmdClassifier::Write));
        assert_eq!(header.msg_counter, Some(counter));
        assert_eq!(header.address_destination.as_ref(), Some(&self.server));
        assert_eq!(header.ack_request, Some(true));
        assert!(
            adapter.poll_transmit().is_none(),
            "one authorized request queues one write"
        );
        let elapsed = Duration::from_secs(now.elapsed_seconds);
        let write = over_wire(&packet);
        self.device.handle_datagram(&write, elapsed);
        while let Some(event) = self.device.poll_event() {
            self.actor.handle_event(&mut self.device, &event, elapsed);
        }
        let mut completion = None;
        while let Some(packet) = self.device.poll_transmit() {
            let packet = over_wire(&packet);
            if let Some(done) = adapter.handle_datagram(&packet, now).unwrap() {
                let header = packet.header.as_ref().unwrap();
                assert_eq!(header.cmd_classifier, Some(CmdClassifier::Result));
                assert_eq!(header.msg_counter_reference, Some(counter));
                assert!(completion.replace(done).is_none());
            }
        }
        (
            completion.expect("correlated response to the adapter's write"),
            adapter.into_engine().unwrap(),
            write,
        )
    }
}

fn over_wire(packet: &Datagram) -> Datagram {
    model::from_json_str(&model::to_json(packet).unwrap()).unwrap()
}

fn directed_limit(power: &Power<Limit>, direction: Direction) -> &Limit {
    let (selected, other) = match direction {
        Direction::Consumption => (&power.consumption, &power.production),
        Direction::Production => (&power.production, &power.consumption),
    };
    assert!(other.is_none(), "the opposite direction must be untouched");
    selected
        .as_ref()
        .expect("limit under the selected direction")
}

fn readback(
    backend: &mut Backend,
    device: &mut LocalDevice,
    route: &str,
    now: Clock,
) -> Option<Power<Limit>> {
    let id = format!("read-{}", now.elapsed_seconds);
    let read = backend.read(&id, vec!["limits".into()]).unwrap();
    let read = Message::decode(&read.encode().unwrap()).unwrap();
    let DeviceEvent::Reply(reply) = device
        .receive(route, &read, &format!("state-{}", now.elapsed_seconds), now)
        .unwrap()
    else {
        panic!("expected state")
    };
    assert_eq!(reply.relation.as_deref(), Some(id.as_str()));
    let received = Message::decode(&reply.encode().unwrap()).unwrap();
    assert!(matches!(
        backend.receive(route, &received, "unused", now).unwrap(),
        BackendEvent::StateUpdated
    ));
    backend.state().unwrap().limits.clone()
}

fn authorize(
    backend: &mut Backend,
    device: &mut LocalDevice,
    route: &str,
    id: &str,
    direction: Direction,
    limit: Limit,
    now: Clock,
) -> String {
    let mut power = Power::default();
    match direction {
        Direction::Consumption => power.consumption = Some(limit),
        Direction::Production => power.production = Some(limit),
    }
    let command = backend
        .send_control(
            id,
            Control {
                limits: Some(power),
                ..Default::default()
            },
            now,
        )
        .unwrap();
    let received = Message::decode(&command.encode().unwrap()).unwrap();
    let DeviceEvent::Apply { request_id } = device.receive(route, &received, "reply", now).unwrap()
    else {
        panic!("an equipment decision is required")
    };
    request_id
}

fn assert_ack(
    backend: &mut Backend,
    route: &str,
    result: Completion,
    id: &str,
    expected: u64,
    now: Clock,
) {
    assert_eq!(result.request_id, id);
    let answer = result
        .acknowledgement
        .expect("standalone control acknowledgement");
    let ack = Message::decode(&answer.encode().unwrap()).unwrap();
    assert_eq!(ack.relation.as_deref(), Some(id));
    assert!(matches!(backend.receive(route,&ack,"unused",now).unwrap(),
        BackendEvent::Acknowledged{request_id,error_number} if request_id==id && error_number==expected));
}

fn exchange_limit(direction: Direction, heartbeat: bool) {
    let (route, capability, watts, failsafe) = match direction {
        Direction::Consumption => ("pump", "lpc", 5300, 4100.),
        Direction::Production => ("inverter", "lpp", 1500, 0.),
    };
    let (mut equipment, mut guard) = Equipment::bound(direction, failsafe);
    equipment.read_limits(&mut guard);
    if heartbeat {
        equipment.actor.system_mut().on_heartbeat(Duration::ZERO);
    }
    let now = Clock::new(10_000, 0);
    let mut backend = Backend::new(route, "backend", 8);
    let mut device = LocalDevice::new(route, "controller", true, 8);
    device
        .capabilities("capabilities", vec![capability.into()], now)
        .unwrap();
    let limit = Limit {
        value: watts,
        active: true,
        duration: Some(30),
    };
    let request_id = authorize(
        &mut backend,
        &mut device,
        route,
        "write",
        direction,
        limit,
        now,
    );
    assert!(device.snapshot(now).unwrap().limits.is_none());
    // The equipment receives the write seven seconds after the CLS.EEDI command.
    // Its remaining lifetime must still end at the original 30-second deadline.
    let dispatched = Clock::new(10_007, 7);
    let (result, _, packet) = equipment.apply(guard, &mut device, &request_id, dispatched);
    let command = &packet.payload.unwrap().cmd.unwrap()[0];
    let data = command.data.as_ref().unwrap();
    let model::CmdData::LoadControlLimitListData(list) = data else {
        panic!("expected limit write")
    };
    let row = &list.load_control_limit_data.as_ref().unwrap()[0];
    let signed = if direction == Direction::Production {
        -watts
    } else {
        watts
    };
    assert_eq!(row.value, Some(model::ScaledNumber::whole(signed)));
    let sent = limitation::read_limit_write(data, limitation::LIMIT_ID, direction).unwrap();
    assert_eq!(sent.watts(), watts as f64);
    assert_eq!(sent.duration, Some(Duration::from_secs(23)));
    let accepted = heartbeat;
    let (expected_spine, expected_ack) = if accepted {
        (ErrorNumber::None, 0)
    } else {
        (ErrorNumber::CommandRejected, 3)
    };
    assert_eq!(result.error, expected_spine);
    assert_ack(
        &mut backend,
        route,
        result,
        &request_id,
        expected_ack,
        dispatched,
    );
    let observed = readback(&mut backend, &mut device, route, dispatched);
    if !accepted {
        assert!(observed.is_none());
        assert_eq!(
            equipment.actor.system().effective_limit(),
            limitation::EffectiveLimit::failsafe(failsafe)
        );
        return;
    }
    let accepted = observed.unwrap();
    assert_eq!(
        directed_limit(&accepted, direction),
        &Limit {
            value: watts,
            active: true,
            duration: Some(23)
        }
    );
    for seconds in [29, 30] {
        equipment
            .actor
            .handle_timeout(&mut equipment.device, Duration::from_secs(seconds));
        let power = readback(
            &mut backend,
            &mut device,
            route,
            Clock::new(10_000 + seconds as i64, seconds),
        )
        .unwrap();
        let observed = directed_limit(&power, direction);
        let expected = if seconds < 30 {
            limitation::EffectiveLimit::active(watts as f64)
        } else {
            limitation::EffectiveLimit::None
        };
        assert_eq!(equipment.actor.system().effective_limit(), expected);
        assert_eq!(observed.active, seconds < 30);
        assert_eq!(observed.duration, if seconds < 30 { Some(1) } else { None });
        assert_eq!(observed.value, watts);
    }
}

#[test]
fn consumption_limit_is_acknowledged_and_expires() {
    exchange_limit(lpc::DIRECTION, true);
}
#[test]
fn consumption_limit_without_heartbeat_is_refused() {
    exchange_limit(lpc::DIRECTION, false);
}
#[test]
fn production_limit_is_acknowledged_and_expires() {
    exchange_limit(lpp::DIRECTION, true);
}
#[test]
fn production_limit_without_heartbeat_is_refused() {
    exchange_limit(lpp::DIRECTION, false);
}

#[test]
fn production_request_on_consumption_only_peer_queues_no_write() {
    let (mut peer, mut guard) = Equipment::bound(lpc::DIRECTION, 4100.0);
    peer.read_limits(&mut guard);
    let mut backend = Backend::new("site", "backend", 8);
    let mut local = LocalDevice::new("site", "controller", true, 8);
    let now = Clock::new(100, 0);
    local
        .capabilities("capabilities", vec!["lpp".into()], now)
        .unwrap();
    let request_id = authorize(
        &mut backend,
        &mut local,
        "site",
        "wrong-direction",
        lpp::DIRECTION,
        Limit {
            value: 1500,
            active: true,
            duration: Some(30),
        },
        now,
    );
    {
        let mut adapter =
            LimitAdapter::new(&mut local, guard, peer.client.clone(), peer.server.clone()).unwrap();
        assert!(adapter.dispatch(&request_id, now).is_err());
        assert!(adapter.poll_transmit().is_none());
    }
    assert!(local.snapshot(now).unwrap().limits.is_none());
    assert_eq!(
        peer.actor.system().effective_limit(),
        limitation::EffectiveLimit::failsafe(4100.0)
    );
}

#[test]
fn open_ended_replacement_withdraws_only_the_selected_deadline() {
    let (mut peer, mut guard) = Equipment::bound(lpc::DIRECTION, 4100.0);
    peer.actor.system_mut().on_heartbeat(Duration::ZERO);
    let limit_id = limitation::LIMIT_ID;
    let mut backend = Backend::new("pump", "backend", 8);
    let mut local = LocalDevice::new("pump", "controller", true, 8);
    local
        .capabilities("caps", vec!["lpc".into()], Clock::new(1000, 0))
        .unwrap();

    for (id, seconds, watts, duration) in [("timed", 0, 4200, Some(20)), ("open", 10, 3700, None)] {
        let now = Clock::new(1000 + seconds as i64, seconds);
        let limit = Limit {
            value: watts,
            active: true,
            duration,
        };
        let request = authorize(
            &mut backend,
            &mut local,
            "pump",
            id,
            lpc::DIRECTION,
            limit.clone(),
            now,
        );
        peer.read_limits(&mut guard);
        let (result, returned_guard, write) = peer.apply(guard, &mut local, &request, now);
        guard = returned_guard;
        assert_eq!(result.error, ErrorNumber::None);
        assert_ack(&mut backend, "pump", result, id, 0, now);
        let observed = readback(&mut backend, &mut local, "pump", now).unwrap();
        assert_eq!(directed_limit(&observed, lpc::DIRECTION), &limit);
        if duration.is_none() {
            let commands = write.payload.unwrap().cmd.unwrap();
            let [command] = commands.as_slice() else {
                panic!("expected a single atomic command")
            };
            let [delete, update] = command.filter.as_deref().unwrap() else {
                panic!("delete and partial filters required")
            };
            assert!(delete.is_delete() && update.is_partial());
            assert_eq!(
                delete.selectors.as_deref(),
                Some(
                    &[model::FilterSelectors::LoadControlLimitListDataSelectors(
                        model::LoadControlLimitListDataSelectors {
                            limit_id: Some(limit_id)
                        }
                    )][..]
                )
            );
            assert_eq!(
                delete.elements,
                Some(model::FilterElements::LoadControlLimitDataElements(
                    model::LoadControlLimitDataElements {
                        time_period: Some(model::TimePeriodElements {
                            end_time: Some(eebus::codec::ElementTag),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }
                ))
            );
        }
    }
    peer.actor
        .handle_timeout(&mut peer.device, Duration::from_secs(21));
    assert_eq!(
        peer.actor.system().effective_limit(),
        limitation::EffectiveLimit::active(3700.0)
    );
    let observed = readback(&mut backend, &mut local, "pump", Clock::new(1021, 21)).unwrap();
    assert_eq!(
        directed_limit(&observed, lpc::DIRECTION),
        &Limit {
            value: 3700,
            active: true,
            duration: None
        }
    );
}
