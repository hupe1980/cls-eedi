//! MQTT routing and independent-broker regressions.
use cls_eedi::transport::{Role, RouteError, Routes, Target};
use cls_eedi::wire::Kind;

#[test]
fn routes_are_exact_and_canonical() {
    let id = "meter/出 %2F+#$\x00\x7f";
    let routes = Routes::new("site/A/in", "site/A/out")
        .unwrap()
        .with_ship(id)
        .unwrap();
    let target = Target::Ship(id.to_owned());
    let topic = routes.topic(Role::Backend, &target).unwrap();
    assert_eq!(topic, "site/A/in/ship/meter%2F出%20%252F%2B%23%24%00%7F");
    assert_eq!(routes.receive(Role::LocalDevice, &topic, false), Ok(target));
    for invalid in [
        "site/B/in",
        "site/A/input",
        "site/A/in/ship/other",
        "site/A/in/ship/%4D",
        "site/A/in/ship/%2f",
        "site/A/in/ship/a/b",
        "site/A/in/ship/%",
    ] {
        assert!(routes.receive(Role::LocalDevice, invalid, false).is_err());
    }
    for invalid in [
        "",
        "site/A/#",
        "site/A/",
        "site//A",
        "x\0y",
        "$share/g/topic",
        "site/A/in",
        "site/A",
        "site/A/in/nested",
    ] {
        assert!(Routes::new("site/A/in", invalid).is_err());
    }
    assert!(routes.with_ship("x".repeat(65535)).is_err());
}

#[test]
fn only_presence_can_be_retained_and_only_on_its_route() {
    let routes = Routes::new("d/a", "u/a").unwrap();
    assert_eq!(
        routes.receive(Role::LocalDevice, "d/a", true),
        Err(RouteError::Retain)
    );
    assert!(routes
        .receive(Role::LocalDevice, "d/a/deviceStatus", true)
        .is_err());
    assert!(routes
        .receive(Role::Backend, "u/a/deviceStatus", false)
        .is_err());
    assert_eq!(
        routes.receive(Role::Backend, "u/a/deviceStatus", true),
        Ok(Target::Presence)
    );
    assert!(Routes::check_kind(Role::LocalDevice, &Target::Presence, Kind::State).is_err());
    assert!(Routes::check_kind(Role::LocalDevice, &Target::Local, Kind::Control).is_err());
}

#[cfg(feature = "mqtt")]
mod broker {
    use super::*;
    use cls_eedi::transport::mqtt::{Broker, Error, Session, SessionEvent};
    use cls_eedi::wire::{Message, Payload, Read};
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    const STEP: Duration = Duration::from_millis(30);
    fn until(mut check: impl FnMut() -> bool) {
        let start = Instant::now();
        while !check() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "broker event timed out"
            );
        }
    }
    fn ready(session: &mut Session) {
        until(|| {
            matches!(
                session.poll(Duration::from_secs(5)).unwrap(),
                Some(SessionEvent::Ready)
            )
        });
    }
    fn presence(session: &mut Session, mut progress: impl FnMut()) -> (String, bool, Option<i64>) {
        let mut result = None;
        until(|| {
            progress();
            if let Some(SessionEvent::Message {
                target: Target::Presence,
                message,
            }) = session.poll(STEP).unwrap()
            {
                if let Payload::LocalDeviceStatus(s) = &message.payload {
                    result = Some((message.id.clone(), s.connected, s.timestamp));
                }
            }
            result.is_some()
        });
        result.unwrap()
    }

    #[test]
    fn plaintext_is_restricted_to_numeric_loopback() {
        assert!(Broker::loopback("test", IpAddr::V4(Ipv4Addr::UNSPECIFIED), 1883).is_err());
        assert!(Broker::loopback("", IpAddr::V4(Ipv4Addr::LOCALHOST), 1883).is_err());
    }

    /// Provision the disposable CA and broker with scripts/check-mqtt.sh.
    #[test]
    #[ignore = "requires the mutual TLS broker configured by scripts/check-mqtt.sh"]
    fn independent_mqtt5_mutual_tls() {
        let dir = std::path::PathBuf::from(
            std::env::var("CLS_EEDI_TEST_PKI").expect("set CLS_EEDI_TEST_PKI"),
        );
        let pem = |name: &str| std::fs::read(dir.join(name)).unwrap();
        let port = std::env::var("CLS_EEDI_TLS_PORT")
            .expect("set CLS_EEDI_TLS_PORT")
            .parse()
            .unwrap();
        let make = |role, ca: &str, authenticate: bool| {
            let client_id = format!("tls-{}", Uuid::new_v4());
            let identity = match role {
                Role::Backend => "backend",
                Role::LocalDevice => "device-a",
            };
            let cert = authenticate.then(|| {
                (
                    pem(&format!("{identity}.crt")),
                    pem(&format!("{identity}.key")),
                )
            });
            let broker = Broker::tls(&client_id, "127.0.0.1", port, pem(ca), cert).unwrap();
            Session::new(
                broker,
                role,
                Routes::new("grid/down/A", "grid/up/A").unwrap(),
                "mtls",
            )
            .unwrap()
        };
        let mut device = make(Role::LocalDevice, "ca.crt", true);
        ready(&mut device);
        let mut backend = make(Role::Backend, "ca.crt", true);
        ready(&mut backend);
        let (_, connected, timestamp) = presence(&mut backend, || {
            device.poll(STEP).unwrap();
        });
        assert!(connected && timestamp.is_some());
        device.disconnect(Duration::from_secs(5)).unwrap();
        let (_, connected, timestamp) = presence(&mut backend, || {});
        assert!(!connected && timestamp.is_none());
        backend.disconnect(Duration::from_secs(5)).unwrap();
        for (ca, identity) in [("untrusted.crt", true), ("ca.crt", false)] {
            let mut rejected = make(Role::Backend, ca, identity);
            until(|| match rejected.poll(Duration::from_secs(5)) {
                Err(Error::Connection(_)) => true,
                Ok(None) => false,
                other => panic!("unexpected TLS authentication result: {other:?}"),
            });
        }
    }

    /// Cross identity checks deliberately use a raw client to bypass our local router.
    #[test]
    #[ignore = "requires the identity-bound EMQX configuration in scripts/check-mqtt.sh"]
    fn independent_mqtt5_certificate_acls() {
        use cls_eedi::wire::{Control, Meter, Notify, State};
        use rumqttc::{
            Client, Event, Incoming, MqttOptions, PubAckReason, PublishOptions, QoS,
            SubscribeReasonCode, Transport,
        };
        let dir = std::path::PathBuf::from(std::env::var("CLS_EEDI_TEST_PKI").unwrap());
        let pem = |file: &str| std::fs::read(dir.join(file)).unwrap();
        let identity = |cn: &str| Some((pem(&format!("{cn}.crt")), pem(&format!("{cn}.key"))));
        let port = std::env::var("CLS_EEDI_TLS_PORT").unwrap().parse().unwrap();
        let make = |cn: &str, role, site| {
            let broker = Broker::tls(cn, "127.0.0.1", port, pem("ca.crt"), identity(cn)).unwrap();
            let routes =
                Routes::new(format!("grid/down/{site}"), format!("grid/up/{site}")).unwrap();
            Session::new(broker, role, routes, cn).unwrap()
        };
        let mut device_b = make("device-b", Role::LocalDevice, "B");
        let mut backend = make("backend", Role::Backend, "B");
        ready(&mut backend);
        ready(&mut device_b);
        let (_, online, _) = presence(&mut backend, || {
            device_b.poll(STEP).unwrap();
        });
        assert!(online);

        // A supplies B's MQTT client ID and the backend's CONNECT username.
        // EMQX maps both fields from the certificate it verified during mTLS.
        let mut options = MqttOptions::new("device-b", ("127.0.0.1", port));
        options.set_transport(Transport::tls(pem("ca.crt"), identity("device-a"), None));
        options.set_credentials("backend", "unused");
        let (client, mut connection) = Client::builder(options).capacity(8).build();
        assert!(matches!(
            connection.recv_timeout(Duration::from_secs(5)),
            Ok(Ok(Event::Incoming(Incoming::ConnAck(_))))
        ));
        for (filter, expected) in [
            ("grid/up/B/#", SubscribeReasonCode::NotAuthorized),
            ("grid/up/#", SubscribeReasonCode::NotAuthorized),
            (
                "grid/down/A/#",
                SubscribeReasonCode::Success(QoS::AtLeastOnce),
            ),
        ] {
            client.try_subscribe(filter, QoS::AtLeastOnce).unwrap();
            until(|| match connection.recv_timeout(STEP) {
                Ok(Ok(Event::Incoming(Incoming::SubAck(ack)))) => {
                    assert_eq!(ack.return_codes, [expected]);
                    true
                }
                Ok(Err(error)) => panic!("ACL connection failed: {error}"),
                _ => false,
            });
        }
        let attempted = Message::new(
            "denied-control",
            "backend",
            Payload::Control(Control {
                notify: Some(Notify::default()),
                ..Control::default()
            }),
        );
        client
            .try_publish(
                "grid/down/B",
                attempted.encode().unwrap(),
                PublishOptions::at_least_once(),
            )
            .unwrap();
        until(|| match connection.recv_timeout(STEP) {
            Ok(Ok(Event::Incoming(Incoming::PubAck(ack)))) => {
                assert_eq!(ack.reason, PubAckReason::NotAuthorized);
                true
            }
            Ok(Err(error)) => panic!("ACL connection failed: {error}"),
            _ => false,
        });

        let read = Message::new("allowed-B", "backend", Payload::Read(Read::default()));
        backend.publish(&Target::Local, &read).unwrap();
        until(|| {
            backend.poll(STEP).unwrap();
            if let Some(SessionEvent::Message { message, .. }) = device_b.poll(STEP).unwrap() {
                assert_eq!(
                    *message, read,
                    "B must receive only its authorized backend request"
                );
                true
            } else {
                false
            }
        });
        let state = Message::new(
            "observation-B",
            "device-b",
            Payload::State(State {
                timestamp: 1_800_000_000,
                measurements: Some(vec![Meter {
                    id: "B-grid".into(),
                    source: "gcp".into(),
                    ..Meter::default()
                }]),
                ..State::default()
            }),
        )
        .related_to(&read.id);
        device_b.publish(&Target::Local, &state).unwrap();
        until(|| {
            device_b.poll(STEP).unwrap();
            if let Some(SessionEvent::Message { message, .. }) = backend.poll(STEP).unwrap() {
                assert_eq!(*message, state);
                true
            } else {
                false
            }
        });

        // A can still receive on its allowed route. The first publication after
        // the denials must be the marker, without B's preceding telemetry.
        backend.disconnect(Duration::from_secs(2)).unwrap();
        let mut backend_a = make("backend", Role::Backend, "A");
        ready(&mut backend_a);
        let marker = Message::new("allowed-A", "backend", Payload::Read(Read::default()));
        backend_a.publish(&Target::Local, &marker).unwrap();
        until(|| {
            backend_a.poll(STEP).unwrap();
            match connection.recv_timeout(STEP) {
                Ok(Ok(Event::Incoming(Incoming::Publish(packet)))) => {
                    assert_eq!(&packet.topic[..], b"grid/down/A");
                    assert_eq!(Message::decode(&packet.payload).unwrap(), marker);
                    true
                }
                Ok(Err(error)) => panic!("ACL connection failed: {error}"),
                _ => false,
            }
        });
    }

    /// Run against a disposable, anonymous MQTT 5 broker bound to loopback.
    #[test]
    #[ignore = "requires a real broker; set CLS_EEDI_BROKER_PORT and pass --ignored"]
    fn independent_mqtt5_broker() {
        use rumqttc::PublishOptions;
        use rumqttc::{Client, Event, Incoming, MqttOptions};
        let port: u16 = std::env::var("CLS_EEDI_BROKER_PORT")
            .expect("set CLS_EEDI_BROKER_PORT")
            .parse()
            .unwrap();
        let prefix = format!("eedi-tests/{}", Uuid::new_v4());
        let down = format!("{prefix}/down");
        let routes = Routes::new(&down, format!("{prefix}/up"))
            .unwrap()
            .with_ship("abc/%")
            .unwrap();
        let make = |role, tag: &str| {
            let id = format!("test-{tag}-{}", Uuid::new_v4());
            let broker = Broker::loopback(&id, Ipv4Addr::LOCALHOST.into(), port).unwrap();
            Session::new(broker, role, routes.clone(), tag).unwrap()
        };
        let mut backend = make(Role::Backend, "backend");
        let mut device = make(Role::LocalDevice, "device");
        ready(&mut device);
        // Flush the retained online PUBLISH before the backend even subscribes.
        device.poll(STEP).unwrap();
        ready(&mut backend);
        let (online_id, connected, timestamp) = presence(&mut backend, || {
            device.poll(STEP).unwrap();
        });
        assert!(connected && timestamp.is_some());

        let target = Target::Ship("abc/%".into());
        let read = Message::new("query", "backend", Payload::Read(Read::default()));
        backend.publish(&target, &read).unwrap();
        until(|| {
            backend.poll(STEP).unwrap();
            match device.poll(STEP).unwrap() {
                Some(SessionEvent::Message {
                    target: actual,
                    message,
                }) => {
                    assert_eq!(actual, target);
                    assert_eq!(*message, read);
                    true
                }
                _ => false,
            }
        });

        // Bypass the send guard with a separate MQTT client to exercise receiver
        // admission for a *live* retained publication.
        let id = format!("test-producer-{}", Uuid::new_v4());
        let (client, mut connection) = Client::builder(MqttOptions::new(id, ("127.0.0.1", port)))
            .capacity(4)
            .build();
        let mut send_raw = |retain: bool, bytes: Vec<u8>| {
            client
                .try_publish(
                    down.as_str(),
                    bytes,
                    PublishOptions::at_least_once().retain(retain),
                )
                .unwrap();
            until(|| {
                matches!(
                    connection.recv_timeout(STEP),
                    Ok(Ok(Event::Incoming(Incoming::PubAck(_))))
                )
            });
        };
        let raw: serde_json::Value = serde_json::from_slice(&read.encode().unwrap()).unwrap();
        // An incompatible acknowledgement must produce no response. Publish it
        // ahead of a bad read on the same connection: the first response must
        // be the ack for the read.
        let mut incompatible_ack = raw.clone();
        incompatible_ack["id"] = "old-ack".into();
        incompatible_ack["relation"] = "previous".into();
        incompatible_ack["type"] = "de.keo-connectivity.clseedi.ack".into();
        incompatible_ack["data"] = serde_json::json!({"protocol":"2.0.0","errorNumber":0});
        send_raw(false, serde_json::to_vec(&incompatible_ack).unwrap());
        until(|| {
            matches!(
                device.poll(STEP).unwrap(),
                Some(SessionEvent::Invalid { .. })
            )
        });
        for (id, data, code) in [
            ("wrong-major", serde_json::json!({"protocol":"2.0.0"}), 2u64),
            (
                "bad-shape",
                serde_json::json!({"protocol":"1.2.0","parameters":5}),
                1u64,
            ),
        ] {
            let mut invalid = raw.clone();
            invalid["id"] = id.into();
            invalid["data"] = data;
            send_raw(false, serde_json::to_vec(&invalid).unwrap());
            until(|| {
                matches!(
                    device.poll(STEP).unwrap(),
                    Some(SessionEvent::Invalid { .. })
                )
            });
            until(|| {
                device.poll(STEP).unwrap();
                if let Some(SessionEvent::Message { target, message }) = backend.poll(STEP).unwrap()
                {
                    assert_eq!(target, Target::Local);
                    assert_eq!(message.relation.as_deref(), Some(id));
                    assert!(
                        matches!(&message.payload, Payload::Ack(ack) if ack.error_number == code)
                    );
                    true
                } else {
                    false
                }
            });
        }
        send_raw(true, read.encode().unwrap());
        until(|| matches!(device.poll(STEP), Err(Error::Route(RouteError::Retain))));

        drop(device);
        let (offline_id, connected, timestamp) = presence(&mut backend, || {});
        assert!(!connected && timestamp.is_none());
        assert_ne!(online_id, offline_id);

        // A fresh subscription receives the same stored offline will.
        let mut observer = make(Role::Backend, "observer");
        ready(&mut observer);
        assert_eq!(presence(&mut observer, || {}), (offline_id, false, None));
        let mut reconnected = make(Role::LocalDevice, "device");
        ready(&mut reconnected);
        // Retained control is rejected on subscription replay too.
        until(|| {
            matches!(
                reconnected.poll(STEP),
                Err(Error::Route(RouteError::Retain))
            )
        });
        let (reconnected_id, connected, timestamp) = presence(&mut backend, || {
            reconnected.poll(STEP).unwrap();
        });
        assert!(connected && timestamp.is_some());
        assert_ne!(reconnected_id, online_id);
    }
}
