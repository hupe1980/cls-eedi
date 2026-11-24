//! Recovering a backend preserves confirmed intent and correlation without republishing work.
use cls_eedi::engine::{Backend, BackendEvent, Clock, EngineError};
use cls_eedi::wire::{
    Ack, Control, Limit, Message, Notify, Payload, Power, Read, State, TrustEntry,
};
use serde_json::{json, Value};

const ROUTE: &str = "home/ship/charger";
fn fresh(capacity: usize) -> Backend {
    Backend::new(ROUTE, "operator", capacity)
}
fn time(t: u64) -> Clock {
    Clock::new(1000 + t as i64, t)
}
fn limit(watts: i64, production: bool, lifetime: u32) -> Control {
    let value = Limit {
        value: watts,
        active: true,
        duration: Some(lifetime),
    };
    let power = if production {
        Power {
            consumption: None,
            production: Some(value),
        }
    } else {
        Power {
            consumption: Some(value),
            production: None,
        }
    };
    Control {
        limits: Some(power),
        ..Default::default()
    }
}
fn ack(id: &str, code: u64) -> Message {
    Message::new(id, "device", Payload::Ack(Ack { error_number: code }))
}
fn confirm(backend: &mut Backend, id: &str, control: Control, t: u64) {
    backend.send_control(id, control, time(t)).unwrap();
    assert!(matches!(
        backend
            .receive(ROUTE, &ack(id, 0), "unused", time(t))
            .unwrap(),
        BackendEvent::Acknowledged {
            error_number: 0,
            ..
        }
    ));
}
fn read_settings(id: &str) -> Message {
    Message::new(id, "device", Payload::Read(Read::default()))
}

#[test]
fn sr_003_restart_keeps_six_independent_setting_scopes_and_ages_both_limit_directions() {
    let mut b = fresh(4);
    confirm(&mut b, "in", limit(6000, false, 30), 2);
    confirm(&mut b, "out", limit(8000, true, 70), 3);
    confirm(
        &mut b,
        "failsafe-in",
        Control {
            failsafes: Some(Power {
                consumption: Some(4600),
                production: None,
            }),
            ..Default::default()
        },
        4,
    );
    confirm(
        &mut b,
        "failsafe-out",
        Control {
            failsafes: Some(Power {
                consumption: None,
                production: Some(7200),
            }),
            ..Default::default()
        },
        5,
    );
    let trust = vec![TrustEntry {
        ski: Some("11".repeat(20)),
        ..Default::default()
    }];
    confirm(
        &mut b,
        "trust",
        Control {
            trust: Some(trust.clone()),
            ..Default::default()
        },
        6,
    );
    let notify = Notify {
        source: Some(vec!["gcp".into()]),
        interval: Some(10),
        end_time: Some(1720),
    };
    confirm(
        &mut b,
        "notify",
        Control {
            notify: Some(notify.clone()),
            ..Default::default()
        },
        7,
    );
    let bytes = b.checkpoint(time(8)).unwrap();
    let sample = Clock::new(1041, 0);
    let mut resumed = fresh(4).restore(&bytes, sample).unwrap();
    let desired = resumed.confirmed_settings(sample).unwrap();
    let directions = desired.limits.unwrap();
    assert_eq!(
        directions.consumption,
        Some(Limit {
            value: 6000,
            active: false,
            duration: None
        })
    );
    assert_eq!(
        directions.production,
        Some(Limit {
            value: 8000,
            active: true,
            duration: Some(32)
        })
    );
    assert_eq!(
        desired.failsafes,
        Some(Power {
            consumption: Some(4600),
            production: Some(7200)
        })
    );
    assert_eq!(desired.trust, Some(trust));
    assert_eq!(desired.notify, Some(notify));
    assert!(matches!(
        resumed.receive(ROUTE, &read_settings("ask"), "settings", sample),
        Err(EngineError::DesiredState)
    ));
}

#[test]
fn sr_002_unresolved_controls_are_correlated_once_without_resend_and_reconciliation_has_capacity() {
    let mut b = fresh(1);
    let command = limit(7400, false, 35);
    b.send_control("unconfirmed", command.clone(), time(4))
        .unwrap();
    let bytes = b.checkpoint(time(7)).unwrap();
    let restart = Clock::new(1018, 0);
    let mut resumed = fresh(1).restore(&bytes, restart).unwrap();
    assert_eq!(
        resumed.pending_controls().collect::<Vec<_>>(),
        [("unconfirmed", &command)]
    );
    assert!(resumed
        .confirmed_settings(restart)
        .unwrap()
        .limits
        .is_none());
    assert!(matches!(
        resumed.send_control("second", limit(3000, false, 9), restart),
        Err(EngineError::Busy)
    ));
    assert!(matches!(
        resumed.receive(ROUTE, &read_settings("ask"), "settings", restart),
        Err(EngineError::ReconciliationRequired)
    ));
    assert!(matches!(
        resumed.read("unconfirmed", vec![]),
        Err(EngineError::IdConflict)
    ));
    resumed.read("read-current", vec!["limits".into()]).unwrap();
    let observed = Message::new(
        "observed",
        "device",
        Payload::State(State {
            timestamp: 1018,
            ..Default::default()
        }),
    )
    .related_to("read-current");
    assert!(matches!(
        resumed.receive(ROUTE, &observed, "r", restart).unwrap(),
        BackendEvent::StateUpdated
    ));
    assert!(matches!(
        resumed.receive("other", &ack("unconfirmed", 0), "r", restart),
        Err(EngineError::Route)
    ));
    assert!(matches!(
        resumed.receive(
            ROUTE,
            &ack("unconfirmed", 0).related_to("unknown"),
            "r",
            restart
        ),
        Err(EngineError::Unexpected)
    ));
    let ack = ack("unconfirmed", 0);
    assert!(matches!(
        resumed.receive(ROUTE, &ack, "r", restart).unwrap(),
        BackendEvent::Acknowledged {
            error_number: 0,
            ..
        }
    ));
    assert!(matches!(
        resumed.receive(ROUTE, &ack, "r", restart),
        Err(EngineError::Unexpected)
    ));
    assert_eq!(resumed.pending_controls().count(), 0);
    let BackendEvent::Reply(settings) = resumed
        .receive(ROUTE, &read_settings("ask"), "settings", restart)
        .unwrap()
    else {
        panic!()
    };
    let Payload::Control(control) = settings.payload else {
        panic!()
    };
    let active = control.limits.unwrap().consumption.unwrap();
    assert_eq!((active.value, active.duration), (7400, Some(21)));
    let bytes = resumed.checkpoint(Clock::new(1020, 2)).unwrap();
    let mut twice = fresh(1).restore(&bytes, Clock::new(1028, 500)).unwrap();
    assert_eq!(
        twice
            .confirmed_settings(Clock::new(1029, 501))
            .unwrap()
            .limits
            .unwrap()
            .consumption
            .unwrap()
            .duration,
        Some(10)
    );
}

#[test]
fn stale_reads_and_observations_are_dropped_but_negative_acks_leave_confirmed_intent_intact() {
    let mut b = fresh(2);
    confirm(&mut b, "confirmed", limit(4110, false, 40), 1);
    let state = Message::new(
        "observation",
        "meter",
        Payload::State(State {
            timestamp: 1110,
            ..Default::default()
        }),
    );
    b.receive(ROUTE, &state, "r", time(2)).unwrap();
    b.read("prior-read", vec![]).unwrap();
    b.send_control("later", limit(4500, false, 60), time(3))
        .unwrap();
    let bytes = b.checkpoint(time(4)).unwrap();
    let restart = Clock::new(1006, 0);
    let mut resumed = fresh(2).restore(&bytes, restart).unwrap();
    assert!(resumed.state().is_none());
    assert!(matches!(
        resumed.receive(ROUTE, &state.related_to("prior-read"), "r", restart),
        Err(EngineError::Unexpected)
    ));
    assert!(matches!(
        resumed
            .receive(ROUTE, &ack("later", 3), "r", restart)
            .unwrap(),
        BackendEvent::Acknowledged {
            error_number: 3,
            ..
        }
    ));
    assert_eq!(
        resumed
            .confirmed_settings(restart)
            .unwrap()
            .limits
            .unwrap()
            .consumption
            .unwrap()
            .value,
        4110
    );
    resumed.read("new-read", vec![]).unwrap();
    assert!(resumed.cancel("new-read"));
    resumed
        .send_control("cancel-me", limit(6200, true, 20), restart)
        .unwrap();
    assert!(resumed.cancel("cancel-me"));
    assert!(matches!(
        resumed.receive(ROUTE, &ack("cancel-me", 0), "r", restart),
        Err(EngineError::Unexpected)
    ));
}

#[test]
fn sr_001_invalid_checkpoint_policy_timing_and_journal_shapes_fail_closed() {
    let mut b = fresh(1);
    confirm(&mut b, "previous", limit(3400, false, 10), 1);
    b.send_control("pending", limit(4000, true, 5), time(2))
        .unwrap();
    let bytes = b.checkpoint(time(3)).unwrap();
    for other in [
        fresh(2),
        Backend::new("other", "operator", 1),
        Backend::new(ROUTE, "other", 1),
    ] {
        assert!(matches!(
            other.restore(&bytes, time(3)),
            Err(EngineError::Checkpoint)
        ));
    }
    assert!(matches!(
        fresh(1).restore(&bytes, time(2)),
        Err(EngineError::Clock)
    ));
    assert!(matches!(
        b.checkpoint(Clock::new(1002, 4)),
        Err(EngineError::Clock)
    ));
    // A parseable record with padding still fails byte-budget admission.
    let mut oversized = bytes.clone();
    oversized.resize(3 * 1024 * 1024, b' ');
    assert!(matches!(
        fresh(1).restore(&oversized, time(4)),
        Err(EngineError::Checkpoint)
    ));
    let original: Value = serde_json::from_slice(&bytes).unwrap();
    let rejects = |doc: Value| {
        assert!(matches!(
            fresh(1).restore(&serde_json::to_vec(&doc).unwrap(), time(4)),
            Err(EngineError::Checkpoint)
        ));
    };
    let mut oversized = bytes.clone();
    oversized.resize(9 * cls_eedi::wire::MAX_MESSAGE_BYTES, b' ');
    assert!(matches!(
        fresh(1).restore(&oversized, time(3)),
        Err(EngineError::Checkpoint)
    ));
    let mut over_capacity = original.clone();
    let mut second = over_capacity["outstanding"][0].clone();
    second[0] = json!("another-command");
    over_capacity["outstanding"]
        .as_array_mut()
        .unwrap()
        .push(second);
    rejects(over_capacity);
    let mut duplicate = original.clone();
    let item = duplicate["confirmed"][0].clone();
    duplicate["confirmed"].as_array_mut().unwrap().push(item);
    rejects(duplicate);
    for (pointer, bad_value) in [
        ("/version", json!(2)),
        ("/tick", json!(0)),
        ("/outstanding/0/1/at", json!(9)),
        ("/outstanding/0/1/at", json!(0)),
        (
            "/confirmed/0/control/limits/power/active/consumption/value",
            json!(-7),
        ),
        ("/outstanding/0/1/control", json!({"unrecognized":true})),
    ] {
        let mut doc = original.clone();
        *doc.pointer_mut(pointer).unwrap() = bad_value;
        rejects(doc);
    }
    let mut extra = original;
    extra["confirmed"][0]["control"]["unknown"] = json!({});
    rejects(extra);
}
