//! Application-level behaviour with an injected clock and an equipment acceptance boundary.
use cls_eedi::{
    engine::{Backend, BackendEvent, Clock, DeviceEvent, EngineError, LocalDevice, Outcome},
    wire::{Ack, Control, Limit, Message, Meter, Notify, Payload, Power, Read, State, TrustEntry},
};

fn clock(seconds: u64) -> Clock {
    Clock::new(1_000 + seconds as i64, seconds)
}
fn consumption(seconds: u32) -> Control {
    Control {
        limits: Some(Power {
            consumption: Some(Limit {
                value: 3500,
                active: true,
                duration: Some(seconds),
            }),
            production: None,
        }),
        ..Default::default()
    }
}
fn message(id: &str, c: Control) -> Message {
    Message::new(id, "operator", Payload::Control(c))
}
fn device(capacity: usize) -> LocalDevice {
    let mut dev = LocalDevice::new("house", "controller", false, capacity);
    dev.capabilities("advert", vec!["lpc".to_owned(), "lpp".to_owned()], clock(0))
        .unwrap();
    dev
}
fn reply(event: DeviceEvent) -> Message {
    if let DeviceEvent::Reply(m) = event {
        *m
    } else {
        panic!("expected a reply")
    }
}
fn code(message: &Message) -> u64 {
    if let Payload::Ack(Ack { error_number }) = message.payload {
        error_number
    } else {
        panic!("expected ack")
    }
}
fn consumption_state(state: State) -> Limit {
    state.limits.unwrap().consumption.unwrap()
}
fn meter(id: &str, source: &str) -> Meter {
    Meter {
        id: id.into(),
        source: source.into(),
        ..Meter::default()
    }
}
fn note(id: &str, wall: i64, meters: Vec<Meter>) -> Message {
    Message::new(
        id,
        "local",
        Payload::State(State {
            measurements: Some(meters),
            timestamp: wall,
            ..Default::default()
        }),
    )
}

#[test]
fn sr_003_timed_command_ack_follows_equipment_and_redelivery_cannot_restart_timer() {
    let mut dev = device(8);
    let mut backend = Backend::new("house", "backend", 8);
    let command = backend
        .send_control("cmd", consumption(12), clock(10))
        .unwrap();
    assert!(matches!(
        dev.receive("house", &command, "spare", clock(10)).unwrap(),
        DeviceEvent::Apply { .. }
    ));
    assert!(matches!(
        dev.receive("house", &command, "spare", clock(11)).unwrap(),
        DeviceEvent::Silent
    ));
    assert!(dev.snapshot(clock(11)).unwrap().limits.is_none());
    let ack = dev
        .complete("cmd", Outcome::Accepted, clock(12))
        .unwrap()
        .unwrap();
    assert_eq!(ack.id, "cmd");
    assert_eq!(ack.relation.as_deref(), Some("cmd"));
    assert!(matches!(
        backend.receive("house", &ack, "spare", clock(12)).unwrap(),
        BackendEvent::Acknowledged {
            error_number: 0,
            ..
        }
    ));
    assert_eq!(
        consumption_state(dev.snapshot(clock(14)).unwrap()).duration,
        Some(8)
    );
    let replay = reply(dev.receive("house", &command, "spare", clock(19)).unwrap());
    assert_eq!(ack, replay);
    let read = backend.read("query", vec!["limits".into()]).unwrap();
    let readback = reply(dev.receive("house", &read, "answer", clock(20)).unwrap());
    backend
        .receive("house", &readback, "spare", clock(20))
        .unwrap();
    assert_eq!(
        consumption_state(backend.state().unwrap().clone()).duration,
        Some(2)
    );
    assert!(!consumption_state(dev.snapshot(clock(22)).unwrap()).active);
}

#[test]
fn sr_003_recovery_preserves_uncertainty_and_elapsed_deadline() {
    let mut dev = device(3);
    let cmd = message("cmd", consumption(60));
    dev.receive("house", &cmd, "reply", clock(25)).unwrap();
    let pending = dev.checkpoint(clock(26)).unwrap();
    assert!(LocalDevice::new("other", "controller", false, 3)
        .restore(&pending, clock(27))
        .is_err());
    assert!(device(3).restore(&pending, Clock::new(1025, 0)).is_err());
    let mut restored = device(3).restore(&pending, Clock::new(1040, 0)).unwrap();
    assert_eq!(
        restored.pending().map(|(id, _)| id).collect::<Vec<_>>(),
        ["cmd"]
    );
    assert!(matches!(
        restored
            .receive("house", &cmd, "reply", Clock::new(1041, 1))
            .unwrap(),
        DeviceEvent::Silent
    ));
    assert!(restored
        .complete("cmd", Outcome::Unknown, Clock::new(1041, 1))
        .unwrap()
        .is_none());
    restored
        .complete("cmd", Outcome::Accepted, Clock::new(1042, 2))
        .unwrap();
    let snapshot = restored.checkpoint(Clock::new(1043, 3)).unwrap();
    let mut twice = device(3).restore(&snapshot, Clock::new(1050, 0)).unwrap();
    assert_eq!(
        consumption_state(twice.snapshot(Clock::new(1051, 1)).unwrap()).duration,
        Some(34)
    );
    assert_eq!(
        code(&reply(
            twice
                .receive("house", &cmd, "r", Clock::new(1055, 5))
                .unwrap()
        )),
        0
    );
}

#[test]
fn sr_003_capacity_and_id_conflict_have_no_equipment_effect() {
    let mut dev = device(1).with_replay_window(10).unwrap();
    let command = message("same", consumption(10));
    dev.receive("house", &command, "x", clock(0)).unwrap();
    assert!(matches!(
        dev.receive("house", &message("same", consumption(2)), "x", clock(2)),
        Err(EngineError::IdConflict)
    ));
    assert!(matches!(
        dev.receive("wrong", &command, "x", clock(2)),
        Err(EngineError::Route)
    ));
    assert!(matches!(
        dev.receive("house", &message("next", consumption(2)), "x", clock(999)),
        Err(EngineError::Capacity)
    ));
    let refused = dev
        .complete("same", Outcome::Rejected, clock(999))
        .unwrap()
        .unwrap();
    assert_eq!(code(&refused), 3);
    assert!(dev.snapshot(clock(999)).unwrap().limits.is_none());
    assert!(matches!(
        dev.receive("house", &message("next", consumption(2)), "x", clock(1008)),
        Err(EngineError::Capacity)
    ));
    assert!(matches!(
        dev.receive("house", &message("next", consumption(2)), "x", clock(1009))
            .unwrap(),
        DeviceEvent::Apply { .. }
    ));
}

#[test]
fn error_codes_and_ambiguous_concurrency_are_explicit() {
    let mut dev = LocalDevice::new("child", "edge", true, 6);
    let cmd = message("unsupported", consumption(1));
    assert_eq!(
        code(&reply(dev.receive("child", &cmd, "x", clock(0)).unwrap())),
        4
    );
    let trust = message(
        "trust",
        Control {
            trust: Some(vec![]),
            ..Default::default()
        },
    );
    assert_eq!(
        code(&reply(dev.receive("child", &trust, "x", clock(0)).unwrap())),
        2
    );
    let read = Message::new(
        "r",
        "up",
        Payload::Read(Read {
            parameters: vec!["trust".into()],
        }),
    );
    assert_eq!(
        code(&reply(dev.receive("child", &read, "x", clock(0)).unwrap())),
        2
    );
    dev.capabilities("advert", vec!["lpc".into()], clock(0))
        .unwrap();
    dev.receive("child", &message("first", consumption(10)), "x", clock(1))
        .unwrap();
    assert_eq!(
        code(&reply(
            dev.receive("child", &message("second", consumption(10)), "x", clock(1))
                .unwrap()
        )),
        3
    );
    assert_eq!(
        code(
            &dev.complete("first", Outcome::Unsupported, clock(1))
                .unwrap()
                .unwrap()
        ),
        4
    );
}

#[test]
fn ack_id_fallback_is_route_scoped_and_explicit_relation_wins() {
    let mut backend = Backend::new("house", "backend", 4);
    backend
        .send_control("cmd", consumption(10), clock(0))
        .unwrap();
    let ack = Message::new("cmd", "device", Payload::Ack(Ack { error_number: 0 }));
    assert!(matches!(
        backend.receive("other", &ack, "x", clock(1)),
        Err(EngineError::Route)
    ));
    assert!(matches!(
        backend.receive("house", &ack.clone().related_to("wrong"), "x", clock(1)),
        Err(EngineError::Unexpected)
    ));
    assert!(matches!(
        backend.receive("house", &ack, "x", clock(1)).unwrap(),
        BackendEvent::Acknowledged {
            error_number: 0,
            ..
        }
    ));
    assert!(matches!(
        backend.receive("house", &ack, "x", clock(1)),
        Err(EngineError::Unexpected)
    ));
}

#[test]
fn local_read_restores_accumulated_control_without_ack_or_restarting_duration() {
    let mut backend = Backend::new("house", "backend", 8);
    let mut dev = device(8);
    let cmd = backend
        .send_control("cmd", consumption(60), clock(1))
        .unwrap();
    dev.receive("house", &cmd, "x", clock(1)).unwrap();
    let ack = dev
        .complete("cmd", Outcome::Accepted, clock(2))
        .unwrap()
        .unwrap();
    backend.receive("house", &ack, "x", clock(2)).unwrap();
    let read = dev.read_control("read").unwrap();
    let BackendEvent::Reply(settings) = backend
        .receive("house", &read, "settings", clock(11))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(settings.relation.as_deref(), Some("read"));
    assert!(matches!(
        dev.receive("house", &settings, "no-ack", clock(11))
            .unwrap(),
        DeviceEvent::Apply { .. }
    ));
    assert!(dev
        .complete("settings", Outcome::Accepted, clock(11))
        .unwrap()
        .is_none());
    assert_eq!(
        consumption_state(dev.snapshot(clock(12)).unwrap()).duration,
        Some(49)
    );
    assert!(matches!(
        dev.receive("house", &settings, "no-ack", clock(12))
            .unwrap(),
        DeviceEvent::Silent
    ));
}

#[test]
fn snapshots_selective_clears_and_meter_updates_have_distinct_semantics() {
    let mut b = Backend::new("house", "backend", 3);
    b.receive(
        "house",
        &note("n1", 1, vec![meter("a", "gcp"), meter("b", "controllable")]),
        "x",
        clock(1),
    )
    .unwrap();
    b.receive(
        "house",
        &note("n2", 2, vec![meter("a", "gcp")]),
        "x",
        clock(2),
    )
    .unwrap();
    assert_eq!(b.state().unwrap().measurements.as_ref().unwrap().len(), 2);
    b.read("q", vec!["limits".into()]).unwrap();
    let state = Message::new(
        "s",
        "device",
        Payload::State(State {
            timestamp: 3,
            ..Default::default()
        }),
    );
    assert!(matches!(
        b.receive(
            "house",
            &note("s", 3, vec![]).related_to("q"),
            "x",
            clock(3)
        ),
        Err(EngineError::Unexpected)
    ));
    b.receive("house", &state.clone().related_to("q"), "x", clock(3))
        .unwrap();
    assert_eq!(b.state().unwrap().measurements.as_ref().unwrap().len(), 2);
    b.read("all", vec![]).unwrap();
    b.receive("house", &state.related_to("all"), "x", clock(3))
        .unwrap();
    assert!(b.state().unwrap().measurements.is_none());
}

#[test]
fn delayed_state_cannot_clear_a_newer_observation_and_oversized_cache_is_atomic() {
    let mut b = Backend::new("house", "backend", 1);
    b.read("q", vec![]).unwrap();
    b.receive(
        "house",
        &note("s", 1, vec![meter("a", "gcp")]),
        "x",
        clock(1),
    )
    .unwrap();
    let late = Message::new(
        "s",
        "device",
        Payload::State(State {
            timestamp: 1,
            ..Default::default()
        }),
    )
    .related_to("q");
    assert!(matches!(
        b.receive("house", &late, "x", clock(1)).unwrap(),
        BackendEvent::Stale
    ));
    assert!(matches!(
        b.receive(
            "house",
            &note("overflow", 2, vec![meter("b", "gcp")]),
            "x",
            clock(2)
        ),
        Err(EngineError::Capacity)
    ));
    assert_eq!(b.state().unwrap().measurements.as_ref().unwrap()[0].id, "a");
    b.read("expire", vec![]).unwrap();
    assert!(b.cancel("expire"));
    assert!(matches!(
        b.receive(
            "house",
            &note("later", 3, vec![]).related_to("expire"),
            "x",
            clock(3)
        ),
        Err(EngineError::Unexpected)
    ));
}

#[test]
fn notify_filters_sources_stops_and_replaces_inventory() {
    let mut d = device(8);
    d.observe_meters(vec![meter("a", "gcp"), meter("b", "controllable")])
        .unwrap();
    let set = message(
        "schedule",
        Control {
            notify: Some(Notify {
                interval: Some(3),
                end_time: Some(1010),
                source: Some(vec!["gcp".into()]),
            }),
            ..Default::default()
        },
    );
    d.receive("house", &set, "x", clock(1)).unwrap();
    d.complete("schedule", Outcome::Accepted, clock(1)).unwrap();
    assert!(d.notification("n", clock(3)).unwrap().is_none());
    let n = d.notification("n", clock(4)).unwrap().unwrap();
    let Payload::State(s) = n.payload else {
        panic!()
    };
    assert_eq!(s.measurements.unwrap(), vec![meter("a", "gcp")]);
    d.observe_meters(vec![]).unwrap();
    assert!(d
        .snapshot(clock(7))
        .unwrap()
        .measurements
        .unwrap()
        .is_empty());
    assert!(d.notification("n", clock(10)).unwrap().is_none());
    let stop = message(
        "cancel",
        Control {
            notify: Some(Notify::default()),
            ..Default::default()
        },
    );
    d.receive("house", &stop, "x", clock(11)).unwrap();
    d.complete("cancel", Outcome::Accepted, clock(11)).unwrap();
    assert!(d.notification("n", clock(100)).unwrap().is_none());
}

#[test]
fn replace_failsafes_per_direction_and_trust_as_one_list() {
    let mut d = device(8);
    for (id, values) in [
        (
            "import",
            Power {
                consumption: Some(31),
                production: None,
            },
        ),
        (
            "export",
            Power {
                consumption: None,
                production: Some(58),
            },
        ),
    ] {
        let m = message(
            id,
            Control {
                failsafes: Some(values),
                ..Default::default()
            },
        );
        d.receive("house", &m, "x", clock(1)).unwrap();
        d.complete(id, Outcome::Accepted, clock(1)).unwrap();
    }
    assert_eq!(
        d.snapshot(clock(1)).unwrap().failsafes.unwrap(),
        Power {
            consumption: Some(31),
            production: Some(58)
        }
    );
    let trusted = vec![TrustEntry {
        ski: Some("11".repeat(20)),
        ..Default::default()
    }];
    let m = message(
        "trust",
        Control {
            trust: Some(trusted.clone()),
            ..Default::default()
        },
    );
    d.receive("house", &m, "x", clock(1)).unwrap();
    d.complete("trust", Outcome::Accepted, clock(2)).unwrap();
    let m = message(
        "clear",
        Control {
            trust: Some(vec![]),
            ..Default::default()
        },
    );
    d.receive("house", &m, "x", clock(2)).unwrap();
    assert!(d
        .complete(
            "clear",
            Outcome::Trusted(vec![cls_eedi::engine::InstalledCertificate {
                certificate: "aa".into(),
                ski: "11".repeat(20)
            }]),
            clock(2)
        )
        .is_err());
    assert_eq!(d.snapshot(clock(2)).unwrap().trust, Some(trusted));
    d.complete("clear", Outcome::Accepted, clock(3)).unwrap();
    assert_eq!(d.snapshot(clock(4)).unwrap().trust, Some(vec![]));
}

#[test]
fn queued_equipment_dispatch_uses_remaining_lifetime_or_skips_expired_command() {
    let mut device = device(2);
    let command = message("queued", consumption(8));
    device
        .receive("house", &command, "reply", clock(10))
        .unwrap();
    let ready = device
        .control_for_dispatch("queued", clock(15))
        .unwrap()
        .unwrap();
    assert_eq!(ready.limits.unwrap().consumption.unwrap().duration, Some(3));
    assert!(matches!(
        device.control_for_dispatch("queued", clock(18)),
        Err(EngineError::ReconciliationRequired)
    ));
    let ack = device
        .complete("queued", Outcome::Rejected, clock(18))
        .unwrap()
        .unwrap();
    assert_eq!(code(&ack), 3);
    assert!(device.snapshot(clock(18)).unwrap().limits.is_none());
}

#[test]
fn receipt_deadline_applies_to_queued_active_and_inactive_limits_in_both_directions() {
    for production in [false, true] {
        for active in [false, true] {
            let mut control = consumption(7);
            let power = control.limits.as_mut().unwrap();
            power.consumption.as_mut().unwrap().active = active;
            if production {
                power.production = power.consumption.take();
            }
            let cmd = message("queued", control.clone());
            let mut local = device(2);
            local.receive("house", &cmd, "unused", clock(3)).unwrap();
            assert!(local
                .control_for_dispatch("queued", clock(10))
                .unwrap()
                .is_none());
            assert!(local.snapshot(clock(10)).unwrap().limits.is_none());
            let ack = local
                .complete("queued", Outcome::Rejected, clock(10))
                .unwrap()
                .unwrap();
            assert_eq!(code(&ack), 3);

            let mut backend = Backend::new("house", "operator", 2);
            backend.send_control("accepted", control, clock(3)).unwrap();
            let ack = Message::new("accepted", "local", Payload::Ack(Ack { error_number: 0 }));
            backend.receive("house", &ack, "unused", clock(5)).unwrap();
            let saved = backend.checkpoint(clock(6)).unwrap();
            let mut resumed = Backend::new("house", "operator", 2)
                .restore(&saved, clock(8))
                .unwrap();
            for (at, remaining) in [(8, Some(2)), (10, None)] {
                let p = resumed
                    .confirmed_settings(clock(at))
                    .unwrap()
                    .limits
                    .unwrap();
                let l = if production {
                    p.production
                } else {
                    p.consumption
                }
                .unwrap();
                assert_eq!(l.duration, remaining);
                assert_eq!(l.active, active && remaining.is_some());
            }
        }
    }
}
