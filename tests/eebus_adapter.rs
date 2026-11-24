#![cfg(feature = "eebus")]

use std::time::Duration;

use cls_eedi::{
    eebus::{AdapterError, LimitAdapter},
    engine::{Clock, EngineError, LocalDevice},
    wire::{Control, Limit, Message, Payload, Power},
};
use eebus::{
    model::{
        self, Cmd, CmdClassifier, CmdData, Datagram, DeviceType, EntityType, ErrorNumber,
        FeatureAddress, FeatureType, Function, Header, LoadControlLimitId, MsgCounter, ResultData,
        Role,
    },
    spine::{Engine, LocalDevice as SpineDevice, LocalEntity, LocalFeature},
    usecases::limitation::{self, Direction},
};

fn clock(t: u64) -> Clock {
    Clock::new(1000 + t as i64, t)
}

// Populate the binding and both snapshots through real SPINE exchanges, including the
// peer-chosen identifier. Tests below vary only the inputs under examination.
fn fixture(desc: CmdData) -> (Engine, FeatureAddress, FeatureAddress) {
    fixture_with_current(desc, Some(vec![current_entry()]))
}

fn current_entry() -> model::LoadControlLimitData {
    model::LoadControlLimitData {
        limit_id: Some(LoadControlLimitId(91)),
        is_limit_changeable: Some(true),
        ..Default::default()
    }
}

fn fixture_with_current(
    desc: CmdData,
    current: Option<Vec<model::LoadControlLimitData>>,
) -> (Engine, FeatureAddress, FeatureAddress) {
    fixture_with_reply(desc, current, |_| {})
}

fn fixture_with_reply(
    desc: CmdData,
    current: Option<Vec<model::LoadControlLimitData>>,
    mut edit: impl FnMut(&mut Datagram),
) -> (Engine, FeatureAddress, FeatureAddress) {
    let mut feature = limitation::load_control_feature(2);
    feature.set_data(desc).unwrap();
    let read_current = current.is_some();
    if let Some(entries) = current {
        feature
            .set_data(CmdData::LoadControlLimitListData(
                model::LoadControlLimitListData {
                    load_control_limit_data: Some(entries),
                },
            ))
            .unwrap();
    }
    let mut remote = SpineDevice::new("i:200", "inverter", DeviceType::Inverter).unwrap();
    remote
        .add_entity(LocalEntity::new([1], EntityType::PVSystem).with_feature(feature))
        .unwrap();
    let server = remote.address_of(&[1], 2);
    let mut peer = Engine::new(remote);
    let mut local =
        SpineDevice::new("i:100", "guard", DeviceType::ElectricitySupplySystem).unwrap();
    local
        .add_entity(
            LocalEntity::new([1], EntityType::GridGuard).with_feature(LocalFeature::new(
                3,
                FeatureType::Generic,
                Role::Client,
            )),
        )
        .unwrap();
    let client = local.address_of(&[1], 3);
    let mut guard = Engine::new(local);
    guard.request_binding(&client, &server, Duration::ZERO);
    let mut exchange = |guard: &mut Engine| {
        while let Some(frame) = guard.poll_transmit() {
            peer.handle_datagram(&frame, Duration::ZERO);
            while let Some(mut reply) = peer.poll_transmit() {
                if reply.header.as_ref().unwrap().cmd_classifier == Some(CmdClassifier::Reply) {
                    edit(&mut reply);
                }
                guard.handle_datagram(&reply, Duration::ZERO);
            }
        }
    };
    exchange(&mut guard);
    while guard.poll_event().is_some() {}
    guard.read(
        &server,
        &client,
        Function::LoadControlLimitDescriptionListData,
        Duration::ZERO,
    );
    if read_current {
        guard.read(
            &server,
            &client,
            Function::LoadControlLimitListData,
            Duration::ZERO,
        );
    }
    exchange(&mut guard);
    (guard, client, server)
}

fn desc() -> CmdData {
    let mut desc = limitation::limit_description(Direction::Production);
    let CmdData::LoadControlLimitDescriptionListData(list) = &mut desc else {
        unreachable!()
    };
    list.load_control_limit_description_data.as_mut().unwrap()[0].limit_id =
        Some(LoadControlLimitId(91));
    desc
}

fn device(active: bool, watts: i64) -> LocalDevice {
    let mut device = LocalDevice::new("inverter", "controller", true, 8);
    device
        .capabilities("caps", vec!["lpp".into()], clock(0))
        .unwrap();
    let control = Control {
        limits: Some(Power {
            consumption: None,
            production: Some(Limit {
                value: watts,
                active,
                duration: Some(30),
            }),
        }),
        ..Default::default()
    };
    device
        .receive(
            "inverter",
            &Message::new("cmd", "operator", Payload::Control(control)),
            "unused",
            clock(0),
        )
        .unwrap();
    device
}

fn result(
    counter: MsgCounter,
    source: &FeatureAddress,
    destination: &FeatureAddress,
    error: Option<ErrorNumber>,
) -> Datagram {
    Datagram {
        header: Some(Header {
            cmd_classifier: Some(CmdClassifier::Result),
            msg_counter: Some(MsgCounter(123)),
            msg_counter_reference: Some(counter),
            address_source: Some(source.clone()),
            address_destination: Some(destination.clone()),
            ..Default::default()
        }),
        payload: Some(model::Payload {
            cmd: Some(vec![Cmd::with_data(CmdData::ResultData(ResultData {
                error_number: error,
                ..Default::default()
            }))]),
        }),
    }
}

#[test]
fn result_requires_transmission_exact_headers_and_explicit_code_before_engine_correlation() {
    let (guard, client, server) = fixture(desc());
    let mut device = device(true, 1500);
    let mut adapter =
        LimitAdapter::new(&mut device, guard, client.clone(), server.clone()).unwrap();
    let counter = adapter.dispatch("cmd", clock(2)).unwrap();
    let valid = result(counter, &server, &client, Some(ErrorNumber::None));
    assert!(adapter.handle_datagram(&valid, clock(2)).unwrap().is_none());
    let frame = adapter.poll_transmit().unwrap();
    let CmdData::LoadControlLimitListData(data) =
        frame.payload.unwrap().cmd.unwrap().remove(0).data.unwrap()
    else {
        unreachable!()
    };
    let limit = &data.load_control_limit_data.unwrap()[0];
    assert_eq!(limit.limit_id, Some(LoadControlLimitId(91)));
    assert_eq!(limit.value, Some(model::ScaledNumber::whole(-1500)));
    assert_eq!(adapter.deadline(), Some(Duration::from_secs(12)));
    let mut wrong_server = server.clone();
    wrong_server.feature = client.feature;
    let mut wrong_client = client.clone();
    wrong_client.entity = Some(vec![model::AddressEntity(9)]);
    let mut wrong_device = server.clone();
    wrong_device.device = client.device.clone();
    let mut wrong_classifier = valid.clone();
    wrong_classifier.header.as_mut().unwrap().cmd_classifier = Some(CmdClassifier::Reply);
    let mut forwarded = valid.clone();
    forwarded.header.as_mut().unwrap().address_originator = Some(wrong_device.clone());
    let mut extra = valid.clone();
    extra
        .payload
        .as_mut()
        .unwrap()
        .cmd
        .as_mut()
        .unwrap()
        .push(Cmd::default());
    for incorrect in [
        wrong_classifier,
        forwarded,
        extra,
        result(counter, &server, &client, None),
        result(counter, &wrong_server, &client, Some(ErrorNumber::None)),
        result(counter, &wrong_device, &client, Some(ErrorNumber::None)),
        result(counter, &server, &wrong_client, Some(ErrorNumber::None)),
        result(
            MsgCounter(counter.0 + 1),
            &server,
            &client,
            Some(ErrorNumber::None),
        ),
    ] {
        assert!(adapter
            .handle_datagram(&incorrect, clock(2))
            .unwrap()
            .is_none());
    }
    let mut simple = valid.clone();
    let header = simple.header.as_mut().unwrap();
    header.address_source.as_mut().unwrap().device = None;
    header.address_destination.as_mut().unwrap().device = None;
    assert!(matches!(
        adapter.handle_datagram(&simple, clock(1)),
        Err(AdapterError::Local(EngineError::Clock))
    ));
    let done = adapter.finish(clock(3)).unwrap().unwrap();
    assert_eq!(done.error, ErrorNumber::None);
    assert_eq!(done.request_id, "cmd");
    assert!(matches!(done.acknowledgement.unwrap().payload,Payload::Ack(a) if a.error_number==0));
    assert!(adapter.handle_datagram(&valid, clock(4)).unwrap().is_none());
    assert!(matches!(
        adapter.dispatch("another", clock(4)),
        Err(AdapterError::Completed)
    ));
    assert!(adapter.into_engine().unwrap().poll_timeout().is_none());
}

#[test]
fn uncertain_handoff_destroys_queued_or_transmitted_write_and_blocks_redispatch() {
    for drain in [false, true] {
        let (guard, client, server) = fixture(desc());
        let mut device = device(true, 1500);
        let mut adapter = LimitAdapter::new(&mut device, guard, client, server).unwrap();
        adapter.dispatch("cmd", clock(0)).unwrap();
        if drain {
            assert!(adapter.poll_transmit().is_some());
        }
        assert!(matches!(
            adapter.into_engine(),
            Err(AdapterError::Outstanding)
        ));
        assert_eq!(device.pending().count(), 1);
        assert!(matches!(
            device.control_for_dispatch("cmd", clock(100)),
            Err(EngineError::ReconciliationRequired)
        ));
        assert!(device.snapshot(clock(100)).unwrap().limits.is_none());
    }
}

#[test]
fn ambiguous_or_wrong_unit_descriptions_preserve_first_dispatch_and_emit_nothing() {
    for case in 0..3 {
        let mut desc = desc();
        let CmdData::LoadControlLimitDescriptionListData(list) = &mut desc else {
            unreachable!()
        };
        let entries = list.load_control_limit_description_data.as_mut().unwrap();
        match case {
            0 => {
                let mut second = entries[0].clone();
                second.limit_id = Some(LoadControlLimitId(92));
                entries.push(second);
            }
            1 => {
                let mut second = entries[0].clone();
                second.limit_direction = Some(model::EnergyDirection::Consume);
                entries.push(second);
            }
            _ => entries[0].unit = None,
        }
        let (guard, client, server) = fixture(desc);
        let mut local = device(true, 10);
        let mut adapter = LimitAdapter::new(&mut local, guard, client, server).unwrap();
        assert!(matches!(
            adapter.dispatch("cmd", clock(1)),
            Err(AdapterError::Limit(_))
        ));
        assert!(adapter.poll_transmit().is_none());
        drop(adapter);
        assert!(local
            .control_for_dispatch("cmd", clock(2))
            .unwrap()
            .is_some());
    }
}

#[test]
fn inactive_timed_write_and_state_share_deadline_across_restart() {
    let (guard, client, server) = fixture(desc());
    let mut local = device(false, i64::MAX);
    let mut adapter = LimitAdapter::new(&mut local, guard, client.clone(), server.clone()).unwrap();
    let counter = adapter.dispatch("cmd", clock(7)).unwrap();
    let frame = adapter.poll_transmit().unwrap();
    let frame = model::from_json_str(&model::to_json(&frame).unwrap()).unwrap();
    let cmd = frame.payload.unwrap().cmd.unwrap().remove(0);
    let CmdData::LoadControlLimitListData(list) = cmd.data.unwrap() else {
        unreachable!()
    };
    let limit = list.load_control_limit_data.unwrap().remove(0);
    assert_eq!(limit.limit_id, Some(LoadControlLimitId(91)));
    assert_eq!(limit.is_limit_active, Some(false));
    assert_eq!(limit.value, Some(model::ScaledNumber::whole(-i64::MAX)));
    assert_eq!(
        limit.time_period.unwrap().end_time.unwrap().as_duration(),
        Some(Duration::from_secs(23))
    );
    assert_eq!(cmd.filter.unwrap().len(), 1);
    adapter
        .handle_datagram(
            &result(counter, &server, &client, Some(ErrorNumber::None)),
            clock(7),
        )
        .unwrap()
        .unwrap();
    drop(adapter);
    let bytes = local.checkpoint(clock(8)).unwrap();
    let mut resumed = LocalDevice::new("inverter", "controller", true, 8)
        .restore(&bytes, Clock::new(1010, 0))
        .unwrap();
    for (utc, expected) in [(1010, Some(20)), (1030, None)] {
        let limit = resumed
            .snapshot(Clock::new(utc, (utc - 1010) as u64))
            .unwrap()
            .limits
            .unwrap()
            .production
            .unwrap();
        assert!(!limit.active);
        assert_eq!(limit.duration, expected);
    }
}

#[test]
fn queued_expired_activations_and_releases_never_emit_writes() {
    for active in [true, false] {
        let (guard, client, server) = fixture(desc());
        let mut local = device(active, 1500);
        let mut adapter = LimitAdapter::new(&mut local, guard, client, server).unwrap();
        assert!(matches!(
            adapter.dispatch("cmd", clock(30)),
            Err(AdapterError::Expired)
        ));
        assert!(adapter.poll_transmit().is_none());
    }
}

#[test]
fn extreme_spine_clock_preserves_dispatch_authorization() {
    let (guard, client, server) = fixture(desc());
    let mut local = device(true, 1500);
    let mut adapter = LimitAdapter::new(&mut local, guard, client, server).unwrap();
    assert!(matches!(
        adapter.dispatch("cmd", Clock::new(1_000, u64::MAX)),
        Err(AdapterError::Local(EngineError::Clock))
    ));
    assert!(adapter.poll_transmit().is_none());
    assert!(adapter.dispatch("cmd", clock(0)).is_ok());
}

#[test]
fn peer_errors_keep_the_original_code_and_preserve_observed_state() {
    for (error, expected) in [
        (ErrorNumber::CommandRejected, 3),
        (ErrorNumber::CommandNotSupported, 4),
        (ErrorNumber::Other(71), 3),
    ] {
        let (guard, client, server) = fixture(desc());
        let mut local = device(true, 1500);
        let mut adapter =
            LimitAdapter::new(&mut local, guard, client.clone(), server.clone()).unwrap();
        let counter = adapter.dispatch("cmd", clock(0)).unwrap();
        adapter.poll_transmit().unwrap();
        let completion = adapter
            .handle_datagram(&result(counter, &server, &client, Some(error)), clock(0))
            .unwrap()
            .unwrap();
        assert_eq!(completion.error, error);
        assert!(
            matches!(completion.acknowledgement.unwrap().payload,Payload::Ack(a) if a.error_number==expected)
        );
        drop(adapter);
        assert!(local.snapshot(clock(0)).unwrap().limits.is_none());
    }
}

#[test]
fn both_complete_lists_and_a_single_explicitly_writable_row_are_required() {
    let (guard, client, server) = fixture_with_current(desc(), None);
    let mut local = device(true, 10);
    assert!(matches!(
        LimitAdapter::new(&mut local, guard, client, server),
        Err(AdapterError::Snapshot)
    ));
    for entries in [
        vec![],
        vec![current_entry(), current_entry()],
        vec![model::LoadControlLimitData {
            is_limit_changeable: None,
            ..current_entry()
        }],
        vec![model::LoadControlLimitData {
            is_limit_changeable: Some(false),
            ..current_entry()
        }],
    ] {
        let (guard, client, server) = fixture_with_current(desc(), Some(entries));
        let mut adapter = LimitAdapter::new(&mut local, guard, client, server).unwrap();
        assert!(matches!(
            adapter.dispatch("cmd", clock(1)),
            Err(AdapterError::Limit(_))
        ));
        assert!(adapter.poll_transmit().is_none());
    }
    assert!(local
        .control_for_dispatch("cmd", clock(2))
        .unwrap()
        .is_some());
}

#[test]
fn an_initial_current_list_fragment_cannot_authorize_a_write() {
    let (guard, client, server) =
        fixture_with_reply(desc(), Some(vec![current_entry()]), |reply| {
            let cmd = &mut reply.payload.as_mut().unwrap().cmd.as_mut().unwrap()[0];
            if matches!(cmd.data, Some(CmdData::LoadControlLimitListData(_))) {
                cmd.filter = Some(vec![model::Filter::partial()]);
            }
        });
    let mut local = device(true, 10);
    assert!(matches!(
        LimitAdapter::new(&mut local, guard, client, server),
        Err(AdapterError::Snapshot)
    ));
    assert!(local
        .control_for_dispatch("cmd", clock(2))
        .unwrap()
        .is_some());
}

#[test]
fn discovery_replies_from_another_feature_cannot_authorize_a_write() {
    let (guard, client, mut server) = fixture(desc());
    server.feature = Some(model::AddressFeature(9));
    let mut local = device(true, 10);
    assert!(matches!(
        LimitAdapter::new(&mut local, guard, client, server),
        Err(AdapterError::Snapshot)
    ));
    assert!(local
        .control_for_dispatch("cmd", clock(2))
        .unwrap()
        .is_some());
}
