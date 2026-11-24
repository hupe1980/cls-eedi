//! A commissioned control unit announces a pairing request to an energy manager,
//! opens a mutually authenticated SHIP session, then reports the manager's SKI.
use cls_eedi::{
    eebus::confirm_pairing,
    engine::{Backend, BackendEvent, Clock, DeviceEvent, EngineError, LocalDevice, Outcome},
    wire::{Control, Message, Pairing, TrustEntry},
};
use eebus::{
    cert::{self, CertParams},
    model::{CmdClassifier, Datagram, Header, MsgCounter, SpecificationVersion},
    runtime::{Node, PairedUnit, TrustStore, TrustedPeer},
    ship::pairing::{
        Nonce, PairingAnnouncement, PairingError, PairingRequest, PairingSecret, Receiver,
        Requester, RequesterAction, SETTLED_AFTER,
    },
    spine::{device_address, node_management},
    tls::{self, ShipTls},
};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo, UnregisterStatus};
use std::{
    net::{IpAddr, Ipv4Addr, UdpSocket},
    time::{Duration, Instant},
};

const BUDGET: Duration = Duration::from_secs(10);
const SERVICE: &str = "_shippairing._tcp.local.";

fn wire(message: &Message) -> Message {
    Message::decode(&message.encode().unwrap()).unwrap()
}

struct Site {
    backend: Backend,
    device: LocalDevice,
    tick: u64,
}

impl Site {
    fn now(&mut self) -> Clock {
        self.tick += 1;
        Clock::new(1800000000 + self.tick as i64, self.tick)
    }

    fn replace(&mut self, pair: Pairing) -> (Pairing, u64) {
        let now = self.now();
        let id = format!("install-{}", self.tick);
        let request = self
            .backend
            .send_control(
                &id,
                Control {
                    trust: Some(vec![TrustEntry {
                        ship_pairing_service: Some(pair),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                now,
            )
            .unwrap();
        let DeviceEvent::Apply { request_id } = self
            .device
            .receive("site", &wire(&request), "unused", now)
            .unwrap()
        else {
            panic!("expected an equipment operation");
        };
        let dispatch = self
            .device
            .control_for_dispatch(&request_id, now)
            .unwrap()
            .unwrap();
        let pair = dispatch
            .trust
            .unwrap()
            .pop()
            .unwrap()
            .ship_pairing_service
            .unwrap();
        let ack = self
            .device
            .complete(&request_id, Outcome::Accepted, now)
            .unwrap()
            .unwrap();
        assert!(
            matches!(self.backend.receive("site", &wire(&ack), "unused", now).unwrap(),
            BackendEvent::Acknowledged {request_id, error_number: 0} if request_id == id)
        );
        (pair, self.device.trust_revision().unwrap())
    }

    fn assert_readback(&mut self, original: &Pairing, expected_ski: Option<eebus::ship::Ski>) {
        let now = self.now();
        let request = self
            .backend
            .read(&format!("read-{}", self.tick), vec!["trust".into()])
            .unwrap();
        let DeviceEvent::Reply(response) = self
            .device
            .receive("site", &wire(&request), "trust-state", now)
            .unwrap()
        else {
            panic!("expected a trust readback");
        };
        assert_eq!(response.relation.as_ref(), Some(&request.id));
        assert!(matches!(
            self.backend
                .receive("site", &wire(&response), "unused", now)
                .unwrap(),
            BackendEvent::StateUpdated
        ));
        let entries = self.backend.state().unwrap().trust.as_ref().unwrap();
        assert_eq!(entries.len(), 1);
        let actual = entries[0].ship_pairing_service.as_ref().unwrap();
        assert_eq!(actual.for_id, original.for_id);
        assert_eq!(actual.for_par, original.for_par);
        assert!(actual.secret == original.secret);
        assert_eq!(
            actual.ski.as_ref().map(|s| s.parse().unwrap()),
            expected_ski
        );
    }
}

// Separate daemons exchange DNS-SD on IPv4 loopback at a private multicast
// port. No LAN interface or multicast service on the usual port is required.
struct LoopbackMdns(ServiceDaemon);

impl LoopbackMdns {
    fn new(port: u16) -> Self {
        let daemon = ServiceDaemon::new_with_port(port).unwrap();
        daemon.disable_interface(IfKind::All).unwrap();
        daemon.enable_interface(IfKind::LoopbackV4).unwrap();
        daemon.set_multicast_loop_v4(true).unwrap();
        Self(daemon)
    }
}

impl Drop for LoopbackMdns {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

struct Announced {
    publisher: LoopbackMdns,
    _unused_srv_socket: tokio::net::TcpSocket,
    fullname: String,
    pairs: Vec<(String, String)>,
}

fn discover(announcement: &PairingAnnouncement) -> Announced {
    let port = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let publisher = LoopbackMdns::new(port);
    let browser = LoopbackMdns::new(port);
    let events = browser.0.browse(SERVICE).unwrap();
    let pairs = announcement.to_pairs();
    // §5.3 reserves a service port that must never accept a connection.
    // Bind without listening so another process cannot acquire the port.
    let unused_srv_socket = tokio::net::TcpSocket::new_v4().unwrap();
    unused_srv_socket
        .bind((Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();
    let srv_port = unused_srv_socket.local_addr().unwrap().port();
    let mut info = ServiceInfo::new(
        SERVICE,
        "test-control-unit-1",
        "test-control-unit.local.",
        &[IpAddr::V4(Ipv4Addr::LOCALHOST)][..],
        srv_port,
        &pairs[..],
    )
    .unwrap();
    info.set_interfaces(vec![IfKind::LoopbackV4]);
    let fullname = info.get_fullname().to_owned();
    publisher.0.register(info).unwrap();
    let until = Instant::now() + BUDGET;
    loop {
        let event = events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .expect("loopback DNS-SD discovery deadline");
        if let ServiceEvent::ServiceResolved(service) = event {
            if service.get_fullname().eq_ignore_ascii_case(&fullname) {
                assert_eq!(service.get_port(), srv_port);
                let pairs = service
                    .get_properties()
                    .iter()
                    .map(|p| (p.key().to_owned(), p.val_str().to_owned()))
                    .collect();
                return Announced {
                    publisher,
                    _unused_srv_socket: unused_srv_socket,
                    fullname,
                    pairs,
                };
            }
        }
    }
}

fn node(id: &str) -> Node {
    let identity = cert::self_signed(CertParams::new(id)).unwrap();
    Node::new(id, ShipTls::new(identity), TrustStore::new())
}

#[tokio::test]
async fn signed_discovery_through_authenticated_ship_to_backend_readback() {
    let local = node("i:32473_u:LocalControl");
    let manager = node("i:32473_u:Manager");
    let mut secret = [0; 16];
    tls::random(&mut secret).unwrap();
    let mut site = Site {
        backend: Backend::new("site", "backend", 8),
        device: LocalDevice::new("site", "control-unit", false, 8),
        tick: 0,
    };
    let (pair, revision) = site.replace(Pairing {
        for_id: manager.ship_id().into(),
        for_par: manager.fingerprint().to_hex(),
        secret: hex::encode(secret),
        ski: None,
    });
    site.assert_readback(&pair, None);

    let mut nonce = [0; Nonce::LEN];
    tls::random(&mut nonce).unwrap();
    let target = pair.for_par.parse().unwrap();
    // Install the target pin before announcing. On this session the receiver
    // may approve only the TLS-proved leaf that matches the configured pin.
    let trust = local.trust_store().clone();
    let target_id = pair.for_id.clone();
    let report = Box::new(move |peer: eebus::runtime::PendingPeer| {
        if peer.fingerprint == target {
            trust.remember(
                TrustedPeer::new(peer.ski)
                    .with_fingerprint(target)
                    .with_ship_id(target_id.clone())
                    .at_level(eebus::ship::TrustLevel::commissioned(32)),
            );
        }
    });
    let request = PairingRequest::new(
        &pair.for_id,
        target,
        local.ship_id(),
        local.fingerprint(),
        Nonce::from_bytes(nonce),
    );
    let mut requester = Requester::new(
        request
            .sign(&PairingSecret::from_hex(&pair.secret).unwrap())
            .unwrap(),
    );
    assert_eq!(requester.poll_action(), Some(RequesterAction::Announce));
    let announcement = discover(requester.announcement());

    let mut wrong_secret = secret;
    wrong_secret[0] ^= 1;
    let mut receiver = Receiver::new(
        manager.ship_id(),
        manager.fingerprint(),
        PairingSecret::new(wrong_secret),
    );
    assert_eq!(
        receiver.evaluate(&announcement.pairs),
        Err(PairingError::DigestMismatch)
    );
    assert!(manager.trust_store().unit().is_none());
    site.assert_readback(&pair, None);
    let mut receiver = Receiver::new(
        manager.ship_id(),
        manager.fingerprint(),
        PairingSecret::new(secret),
    );
    let mut substituted = announcement.pairs.clone();
    let (_, fingerprint) = substituted
        .iter_mut()
        .find(|(key, _)| key == "trustPar")
        .unwrap();
    *fingerprint = manager.fingerprint().to_hex();
    assert!(matches!(
        receiver.evaluate(&substituted),
        Err(PairingError::DigestMismatch)
    ));
    assert!(receiver.guard().entries().is_empty());
    assert!(manager.trust_store().unit().is_none());

    let verified = receiver.evaluate(&announcement.pairs).unwrap();
    manager
        .trust_store()
        .trust_unit(PairedUnit::from_request(&verified));
    assert_eq!(
        receiver.evaluate(&announcement.pairs),
        Err(PairingError::Replay)
    );

    let listener = manager.listen((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let expected_control = local.ski();
    let expected_manager = manager.ski();
    let (request_close, when_to_close) = tokio::sync::oneshot::channel();
    let serve = async {
        let stream = listener.accept().await.unwrap().0;
        let mut connection = manager.accept(stream).await.unwrap();
        assert_eq!(connection.peer(), expected_control);
        let datagram = connection.recv().await.unwrap();
        connection.send(&datagram).await.unwrap();
        when_to_close.await.unwrap();
        connection
            .close(
                eebus::ship::ConnectionCloseReason::Unspecific,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
    };
    let dial = async {
        let mut connection = local
            .connect_reporting(address, Some(report))
            .await
            .unwrap();
        assert_eq!(connection.peer(), expected_manager);
        assert_eq!(connection.peer_fingerprint(), target);
        assert_eq!(
            local
                .trust_store()
                .get(&expected_manager)
                .unwrap()
                .fingerprint,
            Some(target)
        );
        assert!(connection.peer_ship_id().is_none());
        assert!(confirm_pairing(&mut site.device, revision, &connection).is_err());

        // Drive access-methods on both ends. The peer's ID is available only
        // after its answer, which follows the TLS and SHIP handshakes.
        let source = node_management(&device_address("i:32473", "LocalControl").unwrap());
        let read = Datagram {
            header: Some(Header {
                specification_version: Some(SpecificationVersion::from("1.3.0")),
                address_source: Some(source),
                msg_counter: Some(MsgCounter(1)),
                cmd_classifier: Some(CmdClassifier::Read),
                ..Default::default()
            }),
            payload: Some(Default::default()),
        };
        connection.send(&read).await.unwrap();
        assert_eq!(connection.recv().await.unwrap(), read);
        assert_eq!(connection.peer_ship_id(), Some(pair.for_id.as_str()));
        assert!(confirm_pairing(&mut site.device, revision, &connection).unwrap());
        assert!(!confirm_pairing(&mut site.device, revision, &connection).unwrap());
        site.assert_readback(&pair, Some(expected_manager));

        requester.on_connected(Duration::ZERO);
        requester.handle_timeout(SETTLED_AFTER);
        assert_eq!(requester.poll_action(), Some(RequesterAction::Withdraw));
        let removed = announcement
            .publisher
            .0
            .unregister(&announcement.fullname)
            .unwrap();
        assert!(matches!(
            removed.recv_timeout(BUDGET).unwrap(),
            UnregisterStatus::OK
        ));

        let (wrong_id, changed_id) = site.replace(Pairing {
            for_id: "i:32473_u:Other".into(),
            ..pair.clone()
        });
        assert!(matches!(
            confirm_pairing(&mut site.device, changed_id, &connection),
            Err(EngineError::PairingMismatch)
        ));
        site.assert_readback(&wrong_id, None);
        let (replacement, newer) = site.replace(Pairing {
            for_par: local.fingerprint().to_hex(),
            ..pair.clone()
        });
        assert!(matches!(
            confirm_pairing(&mut site.device, newer, &connection),
            Err(EngineError::PairingMismatch)
        ));
        site.assert_readback(&replacement, None);
        // Identical ID and fingerprint: revision alone must reject the old callback.
        let (restored, current) = site.replace(pair.clone());
        assert!(matches!(
            confirm_pairing(&mut site.device, revision, &connection),
            Err(EngineError::PairingMismatch)
        ));
        request_close.send(()).unwrap();
        // Drive SHIP's close announcement through the same connection. Its
        // formerly authenticated peer identity is still available to callers.
        assert!(connection.recv().await.is_err());
        assert!(!connection.is_open());
        assert!(matches!(
            confirm_pairing(&mut site.device, current, &connection),
            Err(EngineError::PairingMismatch)
        ));
        site.assert_readback(&restored, None);
    };
    tokio::time::timeout(BUDGET, async {
        tokio::join!(serve, dial);
    })
    .await
    .expect("loopback SHIP handshake deadline");
}
