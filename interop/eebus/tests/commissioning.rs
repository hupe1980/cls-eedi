use cls_eedi::engine::{
    Backend, BackendEvent, Clock, DeviceEvent, InstalledCertificate, LocalDevice, Outcome,
};
use cls_eedi::wire::{Control, Message, TrustEntry};
use eebus::cert::{self, CertParams};
use eebus::runtime::{TrustStore, TrustedPeer};
use eebus::ship::{Fingerprint, Ski, TrustLevel};

fn roundtrip(message: &Message) -> Message {
    Message::decode(&message.encode().unwrap()).unwrap()
}

fn direct(ski: Ski) -> TrustEntry {
    TrustEntry {
        ski: Some(ski.to_txt_value()),
        ..Default::default()
    }
}

// Resolve the complete replacement first. Failed certificate decoding must
// leave the existing store intact, including records earlier in the request.
fn replace(store: &mut TrustStore, entries: &[TrustEntry]) -> Outcome {
    let next = TrustStore::new();
    let mut installed = Vec::new();
    for entry in entries {
        let record = match (&entry.ski, &entry.certificate) {
            (Some(ski), None) => {
                let Ok(ski) = ski.parse() else {
                    return Outcome::Rejected;
                };
                TrustedPeer::new(ski)
            }
            (None, Some(certificate)) => {
                let Ok(der) = hex::decode(certificate) else {
                    return Outcome::Rejected;
                };
                if cert::uses_ship_curve(&der).is_err() {
                    return Outcome::Rejected;
                }
                let Ok(ski) = cert::ski_from_der(&der) else {
                    return Outcome::Rejected;
                };
                installed.push(InstalledCertificate {
                    certificate: certificate.clone(),
                    ski: ski.to_string(),
                });
                TrustedPeer::new(ski).with_fingerprint(Fingerprint::of_der(&der))
            }
            _ => return Outcome::Rejected,
        };
        next.remember(record.at_level(TrustLevel::commissioned(32)));
    }
    *store = next;
    if installed.is_empty() {
        Outcome::Accepted
    } else {
        Outcome::Trusted(installed)
    }
}

struct Fixture {
    peer: TrustStore,
    backend: Backend,
    device: LocalDevice,
}

impl Fixture {
    fn write(&mut self, entries: Vec<TrustEntry>, tick: u32) -> u64 {
        let now = Clock::new(1000 + i64::from(tick), u64::from(tick));
        let id = format!("control-{tick}");
        let control = self
            .backend
            .send_control(
                &id,
                Control {
                    trust: Some(entries),
                    ..Default::default()
                },
                now,
            )
            .unwrap();
        let event = self
            .device
            .receive("site", &roundtrip(&control), &format!("ack-{tick}"), now)
            .unwrap();
        let DeviceEvent::Apply { request_id } = event else {
            panic!("expected a local trust operation")
        };
        let dispatch = self
            .device
            .control_for_dispatch(&request_id, now)
            .unwrap()
            .unwrap();
        let outcome = replace(&mut self.peer, &dispatch.trust.unwrap());
        let ack = self
            .device
            .complete(&request_id, outcome, now)
            .unwrap()
            .unwrap();
        let ack = roundtrip(&ack);
        assert_eq!(ack.relation.as_deref(), Some(id.as_str()));
        let BackendEvent::Acknowledged {
            request_id,
            error_number,
        } = self.backend.receive("site", &ack, "unused", now).unwrap()
        else {
            panic!("expected an acknowledgement")
        };
        assert_eq!(request_id, id);
        error_number
    }

    fn assert_readback(&mut self, tick: u32, expected: &[Ski]) {
        let now = Clock::new(1000 + i64::from(tick), u64::from(tick));
        let read = self
            .backend
            .read(&format!("read-{tick}"), vec!["trust".into()])
            .unwrap();
        let DeviceEvent::Reply(reply) = self
            .device
            .receive("site", &roundtrip(&read), &format!("state-{tick}"), now)
            .unwrap()
        else {
            panic!("expected a state snapshot")
        };
        let state = roundtrip(&reply);
        assert_eq!(state.relation.as_ref(), Some(&read.id));
        assert!(matches!(
            self.backend.receive("site", &state, "unused", now).unwrap(),
            BackendEvent::StateUpdated
        ));
        let mut actual: Vec<_> = self
            .backend
            .state()
            .unwrap()
            .trust
            .as_ref()
            .unwrap()
            .iter()
            .map(|entry| {
                assert!(entry.certificate.is_none() && entry.ship_pairing_service.is_none());
                entry
                    .ski
                    .as_deref()
                    .unwrap()
                    .parse::<Ski>()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let mut expected: Vec<_> = expected.iter().map(ToString::to_string).collect();
        actual.sort();
        expected.sort();
        let mut peer: Vec<_> = self.peer.all().iter().map(ToString::to_string).collect();
        peer.sort();
        assert_eq!(actual, peer);
        assert_eq!(actual, expected);
    }
}

#[test]
fn certificate_and_direct_ski_replacement_rejection_and_removal() {
    let identity = cert::self_signed(CertParams::new("i:32473_u:inverter")).unwrap();
    let pump = cert::self_signed(CertParams::new("i:32473_u:pump"))
        .unwrap()
        .ski;
    let candidate = cert::self_signed(CertParams::new("i:32473_u:candidate"))
        .unwrap()
        .ski;
    let mut f = Fixture {
        peer: TrustStore::new(),
        backend: Backend::new("site", "backend", 16),
        device: LocalDevice::new("site", "adapter", false, 16),
    };
    let certificate = TrustEntry {
        certificate: Some(hex::encode_upper(identity.certificate_der())),
        ..Default::default()
    };
    assert_eq!(f.write(vec![certificate, direct(pump)], 0), 0);
    let stored = f.peer.get(&identity.ski).unwrap();
    assert_eq!(stored.fingerprint, Some(identity.fingerprint()));
    assert!(f.peer.is_trusted(&identity.ski) && f.peer.is_trusted(&pump));
    f.assert_readback(1, &[identity.ski, pump]);

    // An empty DER SEQUENCE has valid CLS.EEDI hex syntax. The peer cannot
    // read an X.509 identity from it and rejects the whole replacement.
    let invalid = TrustEntry {
        certificate: Some("3000".into()),
        ..Default::default()
    };
    assert_eq!(f.write(vec![direct(candidate), invalid], 2), 3);
    assert!(!f.peer.is_trusted(&candidate));
    assert!(f.peer.is_trusted(&identity.ski) && f.peer.is_trusted(&pump));
    f.assert_readback(3, &[identity.ski, pump]);

    assert_eq!(f.write(vec![direct(pump)], 4), 0);
    assert!(!f.peer.is_trusted(&identity.ski) && f.peer.is_trusted(&pump));
    f.assert_readback(5, &[pump]);
    assert_eq!(f.write(vec![], 6), 0);
    assert!(f.peer.is_empty());
    f.assert_readback(7, &[]);
}
