//! Local trust replacement and the equipment's later, authenticated pairing result.
use cls_eedi::{
    engine::{Clock, DeviceEvent, EngineError, InstalledCertificate, LocalDevice, Outcome},
    wire::{Control, Message, Pairing, Payload, Read, TrustEntry},
};

const ROUTE: &str = "controller";
fn now(t: u64) -> Clock {
    Clock::new(100 + t as i64, t)
}
fn local() -> LocalDevice {
    LocalDevice::new(ROUTE, "edge", false, 8)
        .with_replay_window(2)
        .unwrap()
}
fn direct(ski: &str) -> TrustEntry {
    TrustEntry {
        ski: Some(ski.into()),
        ..Default::default()
    }
}
fn cert(hex: &str) -> TrustEntry {
    TrustEntry {
        certificate: Some(hex.into()),
        ..Default::default()
    }
}
fn pairing() -> TrustEntry {
    TrustEntry {
        ship_pairing_service: Some(Pairing {
            for_id: "Pump/A".into(),
            for_par: "AB".repeat(32),
            secret: "C7".repeat(16),
            ski: None,
        }),
        ..Default::default()
    }
}
fn request(id: &str, entries: Vec<TrustEntry>) -> Message {
    Message::new(
        id,
        "backend",
        Payload::Control(Control {
            trust: Some(entries),
            ..Default::default()
        }),
    )
}
fn receive(d: &mut LocalDevice, message: &Message, t: u64) -> DeviceEvent {
    d.receive(ROUTE, message, "response", now(t)).unwrap()
}
fn read(parameters: &[&str]) -> Message {
    Message::new(
        "read",
        "backend",
        Payload::Read(Read {
            parameters: parameters.iter().map(|p| p.to_string()).collect(),
        }),
    )
}
fn state(d: &mut LocalDevice, t: u64) -> Vec<TrustEntry> {
    let DeviceEvent::Reply(reply) = receive(d, &read(&["trust"]), t) else {
        panic!("expected state")
    };
    let Payload::State(state) = reply.payload else {
        panic!("expected state")
    };
    state.trust.expect("initialized local inventory")
}
fn installed(der: &str, hex: &str) -> InstalledCertificate {
    InstalledCertificate {
        certificate: der.into(),
        ski: hex.into(),
    }
}

#[test]
fn sr_002_certificate_results_are_bound_to_request_and_failure_is_atomic() {
    // DER is opaque to the role engine. The test equipment returns results keyed by
    // its input bytes; the independent SHIP harness separately exercises real X.509.
    let mut d = local();
    let prior = vec![direct(&"11".repeat(20))];
    d.initialize_trust(prior.clone()).unwrap();
    let plain = direct(&"22".repeat(20));
    let first = "a1".repeat(20);
    let second = "b2".repeat(20);
    let control = request(
        "replace",
        vec![cert("30aa"), plain.clone(), pairing(), cert("30bb")],
    );
    assert!(matches!(
        receive(&mut d, &control, 1),
        DeviceEvent::Apply { .. }
    ));
    assert!(matches!(
        d.complete("replace", Outcome::Accepted, now(1)),
        Err(EngineError::TrustReadback)
    ));
    for reports in [
        vec![installed("30aa", &first)],
        vec![installed("30aa", &first), installed("30dd", &second)],
        vec![installed("30aa", &first), installed("30AA", &second)],
        vec![
            installed("30aa", &first),
            installed("30bb", &"z".repeat(40)),
        ],
        vec![
            installed("30aa", &first),
            installed("30bb", &second),
            installed("30cc", &second),
        ],
    ] {
        assert!(matches!(
            d.complete("replace", Outcome::Trusted(reports), now(1)),
            Err(EngineError::TrustReadback)
        ));
        assert_eq!(state(&mut d, 1), prior);
        assert_eq!(d.trust_revision(), Some(1));
    }
    let reports = vec![installed("30BB", &second), installed("30AA", &first)];
    assert!(!format!("{:?}", reports[0]).contains("30BB"));
    assert!(!format!("{:?}", reports[0]).contains(&second));
    let ack = d
        .complete("replace", Outcome::Trusted(reports), now(2))
        .unwrap()
        .unwrap();
    assert!(matches!(ack.payload,Payload::Ack(a) if a.error_number==0));
    assert_eq!(
        state(&mut d, 2),
        vec![direct(&first), plain, pairing(), direct(&second)]
    );
    assert_eq!(d.trust_revision(), Some(2));
    let DeviceEvent::Reply(replayed) = receive(&mut d, &control, 2) else {
        panic!("must replay ACK")
    };
    assert!(matches!(replayed.payload,Payload::Ack(a) if a.error_number==0));
    assert_eq!(d.trust_revision(), Some(2));
}

#[test]
fn sr_002_pairing_progress_preserves_original_credentials_and_rejects_stale_results_after_restart()
{
    let mut d = local();
    let mut desired = pairing();
    let original = desired.clone();
    let replace = request("commission", vec![desired.clone()]);
    receive(&mut d, &replace, 0);
    d.complete("commission", Outcome::Accepted, now(0)).unwrap();
    let revision = d.trust_revision().unwrap();
    let fingerprint = "ab".repeat(32);
    let ski = "cd".repeat(20);
    // Same certificate alone cannot bind a callback to a different SHIP ID.
    assert!(matches!(
        d.pairing_established(revision, "pump/a", &fingerprint, &ski),
        Err(EngineError::PairingMismatch)
    ));
    assert!(matches!(
        d.pairing_established(revision, "Pump/A", &"bb".repeat(32), &ski),
        Err(EngineError::PairingMismatch)
    ));
    assert!(matches!(
        d.pairing_established(revision, "Pump/A", &fingerprint, "bad"),
        Err(EngineError::PairingMismatch)
    ));
    assert_eq!(state(&mut d, 1), vec![original.clone()]);
    // Preserve the active list when equipment refuses a replacement. While the
    // replacement is unresolved an old callback cannot race equipment acceptance.
    receive(&mut d, &request("clear", vec![]), 1);
    assert!(matches!(
        d.pairing_established(revision, "Pump/A", &fingerprint, &ski),
        Err(EngineError::ReconciliationRequired)
    ));
    d.complete("clear", Outcome::Rejected, now(1)).unwrap();
    assert!(d
        .pairing_established(revision, "Pump/A", &fingerprint, &ski)
        .unwrap());
    assert!(!d
        .pairing_established(revision, "Pump/A", &fingerprint, &ski.to_uppercase())
        .unwrap());
    assert!(matches!(
        d.pairing_established(revision, "Pump/A", &fingerprint, &"de".repeat(20)),
        Err(EngineError::PairingMismatch)
    ));
    desired.ship_pairing_service.as_mut().unwrap().ski = Some(ski.clone());
    assert_eq!(state(&mut d, 1), vec![desired]);
    // After the replay horizon the sender can accidentally reuse a wire ID. The new
    // revision still distinguishes asynchronous callbacks for the previous list.
    receive(&mut d, &replace, 3);
    d.complete("commission", Outcome::Accepted, now(3)).unwrap();
    let newer = d.trust_revision().unwrap();
    assert_ne!(newer, revision);
    let bytes = d.checkpoint(now(3)).unwrap();
    let mut d = local().restore(&bytes, now(4)).unwrap();
    assert_eq!(d.trust_revision(), Some(newer));
    assert!(matches!(
        d.pairing_established(revision, "Pump/A", &fingerprint, &ski),
        Err(EngineError::PairingMismatch)
    ));
    assert_eq!(state(&mut d, 4), vec![original]);
    assert!(d
        .pairing_established(newer, "Pump/A", &fingerprint, &ski)
        .unwrap());
    receive(&mut d, &request("revoke", vec![]), 4);
    d.complete("revoke", Outcome::Accepted, now(4)).unwrap();
    assert!(matches!(
        d.pairing_established(newer, "Pump/A", &fingerprint, &ski),
        Err(EngineError::PairingMismatch)
    ));
    assert!(state(&mut d, 4).is_empty());
}

#[test]
fn sr_003_subsequent_full_read_omits_trust_and_explicit_reads_get_code_two() {
    let mut child = LocalDevice::new(ROUTE, "eebus-target", true, 0);
    child
        .capabilities("available", vec!["lpc".into()], now(0))
        .unwrap();
    assert!(child.initialize_trust(vec![pairing()]).is_err());
    for query in [
        read(&["trust"]),
        read(&["limits", "trust"]),
        request("clear", vec![]),
    ] {
        let DeviceEvent::Reply(reply) = receive(&mut child, &query, 0) else {
            panic!()
        };
        assert_eq!(reply.relation.as_deref(), Some(query.id.as_str()));
        assert!(matches!(reply.payload,Payload::Ack(a) if a.error_number==2));
    }
    for query in [read(&[]), read(&["supportedEebusUseCases"])] {
        let DeviceEvent::Reply(reply) = receive(&mut child, &query, 0) else {
            panic!()
        };
        let Payload::State(s) = reply.payload else {
            panic!()
        };
        assert!(s.trust.is_none());
        assert_eq!(s.supported_eebus_use_cases.unwrap(), ["lpc"]);
    }
    let query = child.read_control("desired-read").unwrap();
    let bad_reply = request("related", vec![pairing()]).related_to(query.id);
    assert!(matches!(
        receive(&mut child, &bad_reply, 0),
        DeviceEvent::Silent
    ));
    assert!(child.trust_revision().is_none());
}

#[test]
fn bootstrap_is_single_use_and_ambiguous_pairings_require_a_new_replacement() {
    let mut d = local();
    assert!(d.snapshot(now(0)).unwrap().trust.is_none());
    d.initialize_trust(vec![pairing(), pairing()]).unwrap();
    assert!(d.initialize_trust(vec![]).is_err());
    assert!(matches!(
        d.pairing_established(
            d.trust_revision().unwrap(),
            "Pump/A",
            &"ab".repeat(32),
            &"00".repeat(20)
        ),
        Err(EngineError::PairingMismatch)
    ));
    assert_eq!(state(&mut d, 0), vec![pairing(), pairing()]);
}

#[test]
fn wire_forbids_certificates_in_state_and_backend_claims_of_completed_pairing() {
    let mut pair = pairing();
    pair.ship_pairing_service.as_mut().unwrap().ski = Some("cc".repeat(20));
    assert!(request("premature", vec![pair]).encode().is_err());
    let bad = Message::new(
        "state",
        "edge",
        Payload::State(cls_eedi::wire::State {
            trust: Some(vec![cert("30ab")]),
            ..Default::default()
        }),
    );
    assert!(bad.encode().is_err());
}
