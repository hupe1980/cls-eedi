//! Carry real SPINE reads from one grid meter and two independent heat pumps to a CLS.EEDI backend.
use std::time::Duration;

use cls_eedi::{
    eebus::{meter_snapshot, MeterProfile},
    engine::{Backend, BackendEvent, Clock, DeviceEvent, LocalDevice},
    metering::controllable_total_power,
    wire::{Measurement, Message, Meter, MeterPower, ScaledNumber},
};
use eebus::{
    model::{
        self, CmdData, Datagram, DeviceType, ElectricalConnectionPhaseName as Phase, EntityType,
        FeatureAddress, FeatureType, Function, MeasurementValueState, Role,
    },
    spine::{Engine, LocalDevice as SpineDevice, LocalEntity, LocalFeature, SpineEvent},
    usecases::{
        mgcp,
        monitoring::{Measurand, MonitoredUnit, Quantity},
        mpc,
    },
};

struct Peer {
    unit: MonitoredUnit,
    server: Engine,
    reader: Engine,
    client: FeatureAddress,
    measurement: FeatureAddress,
    connection: FeatureAddress,
    profile: MeterProfile,
    id: &'static str,
}

impl Peer {
    fn new(id: &'static str, profile: MeterProfile, readings: &[(Measurand, f64)]) -> Self {
        let grid = matches!(profile, MeterProfile::Mgcp);
        let (entity, device_type, naming, use_case) = if grid {
            (
                EntityType::GridConnectionPointOfPremises,
                DeviceType::SubMeter,
                mgcp::NAMING,
                &mgcp::GRID_CONNECTION_POINT,
            )
        } else {
            (
                EntityType::HeatPumpAppliance,
                DeviceType::HeatGenerationSystem,
                mpc::NAMING,
                &mpc::MONITORED_UNIT,
            )
        };
        let mut unit = MonitoredUnit::new(1).naming(naming);
        for (measurand, value) in readings {
            unit.add(measurand.clone());
            unit.set(measurand, *value);
        }
        let mut device = SpineDevice::new("i:34200", id, device_type).unwrap();
        device
            .add_entity(
                LocalEntity::new([1], entity)
                    .with_feature(unit.electrical_connection_feature(1))
                    .with_feature(unit.measurement_feature(2)),
            )
            .unwrap();
        let connection = device.address_of(&[1], 1);
        let measurement = device.address_of(&[1], 2);
        let mut server = Engine::new(device);
        unit.publish(&mut server, &connection, &measurement);
        let scenarios: &[u32] = if grid { &[2, 3] } else { &[1, 3] };
        server.add_use_case_scenarios([1], 1, use_case, scenarios);
        let mut device =
            SpineDevice::new("i:34300", "collector", DeviceType::EnergyManagementSystem).unwrap();
        device
            .add_entity(
                LocalEntity::new([1], EntityType::CEM).with_feature(LocalFeature::new(
                    1,
                    FeatureType::Generic,
                    Role::Client,
                )),
            )
            .unwrap();
        let client = device.address_of(&[1], 1);
        Self {
            unit,
            server,
            reader: Engine::new(device),
            client,
            measurement,
            connection,
            profile,
            id,
        }
    }

    fn exchange_read(&mut self, function: Function) -> (Datagram, Datagram) {
        let feature = if matches!(
            function,
            Function::ElectricalConnectionParameterDescriptionListData
                | Function::ElectricalConnectionDescriptionListData
        ) {
            &self.connection
        } else {
            &self.measurement
        };
        let counter = self
            .reader
            .read(feature, &self.client, function, Duration::ZERO);
        let request = over_wire(&self.reader.poll_transmit().expect("SPINE read"));
        let header = request.header.as_ref().unwrap();
        assert_eq!(header.cmd_classifier, Some(model::CmdClassifier::Read));
        assert_eq!(header.msg_counter, Some(counter));
        assert_eq!(header.address_source.as_ref(), Some(&self.client));
        assert_eq!(header.address_destination.as_ref(), Some(feature));
        self.server.handle_datagram(&request, Duration::ZERO);
        let reply = over_wire(&self.server.poll_transmit().expect("equipment reply"));
        (request, reply)
    }

    fn read(&mut self, function: Function) -> CmdData {
        let (request, reply) = self.exchange_read(function);
        assert!(answers(&request, &reply));
        let remote = request.header.unwrap().address_destination.unwrap();
        self.reader.handle_datagram(&reply, Duration::ZERO);
        std::iter::from_fn(|| self.reader.poll_event())
            .find_map(|e| match e {
                SpineEvent::ReplyReceived { feature, data, .. } if feature == remote => Some(data),
                _ => None,
            })
            .expect("expected feature responded")
    }

    fn sample(&mut self) -> Meter {
        self.unit
            .publish(&mut self.server, &self.connection, &self.measurement);
        let descriptions = self.read(Function::MeasurementDescriptionListData);
        let connection = self.read(Function::ElectricalConnectionDescriptionListData);
        let parameters = self.read(Function::ElectricalConnectionParameterDescriptionListData);
        let values = self.read(Function::MeasurementListData);
        meter_snapshot(
            self.profile,
            self.id,
            None,
            &descriptions,
            &connection,
            &parameters,
            &values,
        )
        .unwrap()
    }
}

fn answers(request: &Datagram, reply: &Datagram) -> bool {
    let request = request.header.as_ref().unwrap();
    let reply = reply.header.as_ref().unwrap();
    reply.cmd_classifier == Some(model::CmdClassifier::Reply)
        && reply.address_source == request.address_destination
        && reply.address_destination == request.address_source
        && reply.msg_counter_reference == request.msg_counter
}

fn over_wire(datagram: &Datagram) -> Datagram {
    model::from_json_str(&model::to_json(datagram).unwrap()).unwrap()
}

fn readback(device: &mut LocalDevice, backend: &mut Backend, tick: u32) -> Vec<Meter> {
    let now = Clock::new(1_000 + i64::from(tick), u64::from(tick));
    let read = backend
        .read(&format!("read-{tick}"), vec!["measurements".into()])
        .unwrap();
    let read = Message::decode(&read.encode().unwrap()).unwrap();
    let DeviceEvent::Reply(reply) = device
        .receive("site", &read, &format!("reply-{tick}"), now)
        .unwrap()
    else {
        panic!("state reply expected")
    };
    let received = Message::decode(&reply.encode().unwrap()).unwrap();
    assert_eq!(received.relation.as_ref(), Some(&read.id));
    assert!(matches!(
        backend.receive("site", &received, "unused", now).unwrap(),
        BackendEvent::StateUpdated
    ));
    backend.state().unwrap().measurements.clone().unwrap()
}

fn total(meter: &Meter) -> Option<ScaledNumber> {
    meter
        .power
        .as_ref()?
        .total
        .as_ref()
        .map(|power| power.value.clone())
}

// The two endpoint IDs are provisioned as the non-overlapping, exhaustive
// controllable branches behind this grid meter. Both belong to the same sample
// window. Only total active power is additive under this fixture's topology.
fn publish_window(device: &mut LocalDevice, grid: Meter, loads: &[Meter]) {
    let samples: Vec<_> = loads
        .iter()
        .map(|meter| (meter.id.as_str(), total(meter)))
        .collect();
    let mut observations = vec![grid];
    if let Some(value) = controllable_total_power(&["pump-east", "pump-west"], &samples).unwrap() {
        observations.push(Meter {
            id: "controllable-total".into(),
            source: "controllable".into(),
            power: Some(MeterPower {
                total: Some(Measurement { value }),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    device.observe_meters(observations).unwrap();
}

fn number(number: i64, scale: i16) -> ScaledNumber {
    ScaledNumber { number, scale }
}

#[test]
fn measured_sign_scale_source_phase_and_invalidation_reach_backend() {
    let power = Measurand::total_power();
    let energy = Measurand::total(Quantity::EnergyProduced);
    let current = Measurand::on(Quantity::Current, Phase::A);
    let mut grid = Peer::new(
        "site-boundary",
        MeterProfile::Mgcp,
        &[(energy.clone(), -85.125), (power.clone(), -1275.25)],
    );
    let mut east = Peer::new(
        "pump-east",
        MeterProfile::Mpc,
        &[(power.clone(), 125.125), (current.clone(), 3.75)],
    );
    let mut west = Peer::new(
        "pump-west",
        MeterProfile::Mpc,
        &[(current.clone(), 1.5), (power.clone(), 74.875)],
    );
    // All three peers assign measurement 1 independently to different data.
    assert_eq!(
        east.unit.measurement_id(&power),
        west.unit.measurement_id(&current)
    );
    assert_eq!(
        east.unit.measurement_id(&power),
        grid.unit.measurement_id(&energy)
    );
    let mut backend = Backend::new("site", "backend", 16);
    let mut device = LocalDevice::new("site", "collector", false, 16);
    let e = east.sample();
    let w = west.sample();
    assert_eq!(total(&e), Some(number(125125, -3)));
    assert_eq!(total(&w), Some(number(74875, -3)));
    let phases = e.current.as_ref().unwrap();
    assert_eq!(phases.a.as_ref().unwrap().value, number(375, -2));
    assert!(phases.b.is_none() && phases.c.is_none());
    publish_window(&mut device, grid.sample(), &[e, w]);
    let meters = readback(&mut device, &mut backend, 0);
    assert_eq!(meters.len(), 2);
    let gcp = meters.iter().find(|m| m.source == "gcp").unwrap();
    let combined = meters.iter().find(|m| m.source == "controllable").unwrap();
    assert_eq!(gcp.id, "site-boundary");
    assert_eq!(total(gcp), Some(number(-127525, -2)));
    assert_eq!(
        gcp.energy
            .as_ref()
            .unwrap()
            .produced
            .as_ref()
            .unwrap()
            .value,
        number(-85125, -3)
    );
    assert_eq!(combined.id, "controllable-total");
    assert_eq!(total(combined), Some(number(200000, -3)));
    assert!(combined.current.is_none() && combined.energy.is_none());

    // A missing participant removes the entire aggregate. The two grid
    // quality states each remove power while preserving its valid energy.
    for (tick, state) in [
        (1, MeasurementValueState::Error),
        (2, MeasurementValueState::OutOfRange),
    ] {
        grid.unit.set_state(&power, state);
        publish_window(&mut device, grid.sample(), &[east.sample()]);
        let remaining = readback(&mut device, &mut backend, tick);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "site-boundary");
        assert!(total(&remaining[0]).is_none());
        assert_eq!(
            remaining[0]
                .energy
                .as_ref()
                .unwrap()
                .produced
                .as_ref()
                .unwrap()
                .value,
            number(-85125, -3)
        );
    }
    // Both branches return, but one reports invalid total watts with a numeric
    // payload. Suppress the partial sum until both report usable values.
    west.unit.set_state(&power, MeasurementValueState::Error);
    let invalid = west.sample();
    assert!(total(&invalid).is_none());
    publish_window(&mut device, grid.sample(), &[east.sample(), invalid]);
    assert_eq!(readback(&mut device, &mut backend, 3).len(), 1);
    east.unit.set(&power, 0.0);
    west.unit.set(&power, 0.0);
    west.unit.set_state(&power, MeasurementValueState::Normal);
    publish_window(&mut device, grid.sample(), &[west.sample(), east.sample()]);
    let restored = readback(&mut device, &mut backend, 4);
    let aggregate = restored
        .iter()
        .find(|m| m.source == "controllable")
        .unwrap();
    assert_eq!(total(aggregate).unwrap().to_string(), "0");
}

#[test]
fn equal_read_counters_on_different_peer_sessions_do_not_cross_correlate() {
    let mut east = Peer::new(
        "pump-east",
        MeterProfile::Mpc,
        &[(Measurand::total_power(), 100.0)],
    );
    let mut west = Peer::new(
        "pump-west",
        MeterProfile::Mpc,
        &[(Measurand::total_power(), 300.0)],
    );
    let (east_read, east_reply) = east.exchange_read(Function::MeasurementDescriptionListData);
    let (west_read, west_reply) = west.exchange_read(Function::MeasurementDescriptionListData);
    assert_eq!(
        east_read.header.as_ref().unwrap().msg_counter,
        west_read.header.as_ref().unwrap().msg_counter
    );
    assert!(answers(&east_read, &east_reply) && answers(&west_read, &west_reply));
    assert!(!answers(&east_read, &west_reply) && !answers(&west_read, &east_reply));
}
