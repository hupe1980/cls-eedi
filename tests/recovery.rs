//! Checkpoint recovery against a newly provisioned engine and an independently sampled clock.
use cls_eedi::engine::{Clock, DeviceEvent, EngineError, LocalDevice, Outcome};
use cls_eedi::wire::{Ack, Control, Limit, Message, Meter, Pairing, Payload, Power, TrustEntry};
use serde_json::{json, Value};

fn provision() -> LocalDevice {
    LocalDevice::new("site/pump", "edge", false, 4)
        .with_replay_window(10)
        .unwrap()
}

fn now(second: u64) -> Clock {
    Clock::new(500 + second as i64, second)
}

fn request(id: &str) -> Message {
    Message::new(
        id,
        "grid",
        Payload::Control(Control {
            limits: Some(Power {
                consumption: Some(Limit {
                    active: true,
                    value: 5000,
                    duration: Some(30),
                }),
                production: None,
            }),
            ..Default::default()
        }),
    )
}

fn pending() -> LocalDevice {
    let mut d = provision();
    d.capabilities("advert", vec!["lpc".into()], now(0))
        .unwrap();
    assert!(matches!(
        d.receive("site/pump", &request("work"), "r", now(1))
            .unwrap(),
        DeviceEvent::Apply { .. }
    ));
    d
}

#[test]
fn sr_002_restore_uses_current_policy_and_requires_equipment_reconciliation() {
    let bytes = pending().checkpoint(now(2)).unwrap();
    for mismatched in [
        LocalDevice::new("site/battery", "edge", false, 4),
        LocalDevice::new("site/pump", "another-source", false, 4),
        LocalDevice::new("site/pump", "edge", true, 4),
        LocalDevice::new("site/pump", "edge", false, 3),
    ] {
        assert!(matches!(
            mismatched
                .with_replay_window(10)
                .unwrap()
                .restore(&bytes, now(4)),
            Err(EngineError::Checkpoint)
        ));
    }
    assert!(matches!(
        LocalDevice::new("site/pump", "edge", false, 4).restore(&bytes, now(4)),
        Err(EngineError::Checkpoint)
    ));
    let restart = Clock::new(506, 0);
    let mut d = provision().restore(&bytes, restart).unwrap();
    assert!(matches!(
        d.control_for_dispatch("work", restart),
        Err(EngineError::ReconciliationRequired)
    ));
    assert!(matches!(
        d.receive("site/pump", &request("work"), "r", restart)
            .unwrap(),
        DeviceEvent::Silent
    ));
    assert_eq!(d.pending().count(), 1);
    let observed = d
        .complete("work", Outcome::Accepted, restart)
        .unwrap()
        .unwrap();
    assert_eq!(observed.payload, Payload::Ack(Ack { error_number: 0 }));
    let limit = d
        .snapshot(restart)
        .unwrap()
        .limits
        .unwrap()
        .consumption
        .unwrap();
    assert_eq!(limit.duration, Some(25));
}

#[test]
fn first_dispatch_unknown_and_expired_commands_cannot_repeat_actuation() {
    let mut d = pending();
    assert!(d.control_for_dispatch("work", now(2)).unwrap().is_some());
    assert!(matches!(
        d.control_for_dispatch("work", now(3)),
        Err(EngineError::ReconciliationRequired)
    ));
    let mut d = pending();
    d.complete("work", Outcome::Unknown, now(2)).unwrap();
    assert!(matches!(
        d.control_for_dispatch("work", now(3)),
        Err(EngineError::ReconciliationRequired)
    ));
    let mut d = pending();
    assert!(d.control_for_dispatch("work", now(31)).unwrap().is_none());
    let ack = d
        .complete("work", Outcome::Rejected, now(31))
        .unwrap()
        .unwrap();
    assert_eq!(ack.payload, Payload::Ack(Ack { error_number: 3 }));
}

#[test]
fn downtime_expires_physical_limits_but_preserves_replay_protection() {
    let mut d = pending();
    let ack = d
        .complete("work", Outcome::Accepted, now(2))
        .unwrap()
        .unwrap();
    let bytes = d.checkpoint(now(4)).unwrap();
    let restart = Clock::new(10_000, 77);
    let mut resumed = provision().restore(&bytes, restart).unwrap();
    assert!(
        !resumed
            .snapshot(restart)
            .unwrap()
            .limits
            .unwrap()
            .consumption
            .unwrap()
            .active
    );
    let DeviceEvent::Reply(replayed) = resumed
        .receive("site/pump", &request("work"), "r", Clock::new(10_007, 84))
        .unwrap()
    else {
        panic!("must replay the saved result")
    };
    assert_eq!(*replayed, ack);
    // Exactly two seconds of the ten-second retention window elapsed before shutdown.
    assert!(matches!(
        resumed
            .receive("site/pump", &request("work"), "r", Clock::new(10_008, 85))
            .unwrap(),
        DeviceEvent::Apply { .. }
    ));
}

#[test]
fn restart_invalidates_old_read_relations_and_rejects_clock_regression() {
    let mut d = provision();
    d.read_control("previous-session").unwrap();
    let bytes = d.checkpoint(now(4)).unwrap();
    assert!(matches!(
        d.checkpoint(Clock::new(501, 8)),
        Err(EngineError::Clock)
    ));
    assert!(matches!(
        provision().restore(&bytes, Clock::new(503, 99)),
        Err(EngineError::Clock)
    ));
    let mut resumed = provision().restore(&bytes, Clock::new(505, 0)).unwrap();
    let delayed = request("settings").related_to("previous-session");
    assert!(matches!(
        resumed.receive("site/pump", &delayed, "r", Clock::new(505, 0)),
        Err(EngineError::Unexpected)
    ));
    assert_eq!(resumed.read_control("fresh").unwrap().id, "fresh");
}

fn rejects(doc: &Value) {
    let bytes = serde_json::to_vec(doc).unwrap();
    assert!(matches!(
        provision().restore(&bytes, now(5)),
        Err(EngineError::Checkpoint)
    ));
}

#[test]
fn corrupt_ledgers_semantic_state_and_impossible_deadlines_fail_closed() {
    let mut d = pending();
    let original: Value = serde_json::from_slice(&d.checkpoint(now(3)).unwrap()).unwrap();
    for (pointer, value) in [
        ("/ledger/work/finished", json!(2)),
        ("/ledger/work/received", json!(7)),
        (
            "/ledger/work/execution/Pending/command/limits/power/active/consumption/value",
            json!(-7),
        ),
        ("/deadlines/0", json!(88)),
        ("/next_notification", json!(3)),
        ("/state/timestamp", json!(-1)),
    ] {
        let mut damaged = original.clone();
        *damaged.pointer_mut(pointer).unwrap() = value;
        rejects(&damaged);
    }
    let mut overlap = original.clone();
    overlap["ledger"]["overlap"] = original["ledger"]["work"].clone();
    rejects(&overlap);

    let encoded = serde_json::to_string(&original).unwrap();
    let needle = serde_json::to_string(&original["ledger"]).unwrap();
    let entry = serde_json::to_string(&original["ledger"]["work"]).unwrap();
    let duplicate = encoded.replace(&needle, &format!(r#"{{"work":{entry},"work":{entry}}}"#));
    assert!(matches!(
        provision().restore(duplicate.as_bytes(), now(5)),
        Err(EngineError::Checkpoint)
    ));

    d.complete("work", Outcome::Accepted, now(3)).unwrap();
    let original: Value = serde_json::from_slice(&d.checkpoint(now(4)).unwrap()).unwrap();
    for (pointer, value) in [
        ("/ledger/work/finished", json!(0)),
        ("/ledger/work/execution/Done", json!(99)),
        ("/deadlines/0", Value::Null),
        ("/deadlines/0", json!(100)),
    ] {
        let mut damaged = original.clone();
        *damaged.pointer_mut(pointer).unwrap() = value;
        rejects(&damaged);
    }
}

#[test]
fn state_observations_remain_transactional_when_combined_snapshot_exceeds_wire_budget() {
    let mut d = LocalDevice::new("site", "edge", false, 256);
    let trust = TrustEntry {
        ship_pairing_service: Some(Pairing {
            for_id: "d".repeat(230_000),
            for_par: "aa".repeat(32),
            secret: "11".repeat(16),
            ski: None,
        }),
        ..Default::default()
    };
    d.initialize_trust(vec![trust]).unwrap();
    let meters = (0..200)
        .map(|num| Meter {
            id: format!("meter-{num:03}{}", "x".repeat(220)),
            source: "gcp".into(),
            ..Default::default()
        })
        .collect();
    assert!(d.observe_meters(meters).is_err());
    let bytes = d.checkpoint(now(0)).unwrap();
    let mut recovered = LocalDevice::new("site", "edge", false, 256)
        .restore(&bytes, now(0))
        .unwrap();
    let state = recovered.snapshot(now(0)).unwrap();
    assert!(state.measurements.is_none());
    assert_eq!(state.trust.unwrap().len(), 1);
}
