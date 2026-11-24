//! Synchronous MQTT 5 binding. Drive one session from a dedicated blocking loop.
//! Applications choose their reconnect backoff and reconcile uncertain commands
//! by reading the device after each new `Ready` event.
use super::{Role, RouteError, Routes, Target};
use crate::wire::{Ack, Message, Payload, Status, WireError};
use futures_util::{stream::BoxStream, StreamExt};
use rumqttc::mqttbytes::v5::{LastWill, PubAckReason, SubscribeFilter, SubscribeReasonCode};
use rumqttc::mqttbytes::QoS;
use rumqttc::tokio_rustls::rustls::{
    self,
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    ClientConfig, RootCertStore,
};
use rumqttc::{AsyncClient, ConnectionError, Event, Incoming, MqttOptions, PublishOptions};
use rumqttc::{DisconnectProperties, DisconnectReasonCode, Outgoing, Transport};
use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

/// Broker settings. Credentials and private keys stay out of Debug output.
#[derive(Clone)]
pub struct Broker(MqttOptions);

impl Broker {
    /// Verify the broker against PEM trust anchors. Supply a PEM certificate chain
    /// and private key when mutual TLS is required.
    pub fn tls(
        client_id: &str,
        host: &str,
        port: u16,
        ca: Vec<u8>,
        identity: Option<(Vec<u8>, Vec<u8>)>,
    ) -> Result<Self, Error> {
        if ca.is_empty() || host.is_empty() {
            return Err(Error::Config);
        }
        let mut options = Self::options(client_id, host, port)?;
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(&ca) {
            roots
                .add(cert.map_err(|_| Error::TrustAnchors)?)
                .map_err(|_| Error::TrustAnchors)?;
        }
        if roots.is_empty() {
            return Err(Error::TrustAnchors);
        }
        // A downstream application can also enable the ring provider. Choose
        // this binding's provider explicitly without changing process globals.
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let config = ClientConfig::builder_with_provider(provider.into())
            .with_safe_default_protocol_versions()
            .map_err(|_| Error::Config)?
            .with_root_certificates(roots);
        let config = if let Some((cert, key)) = identity {
            let chain = CertificateDer::pem_slice_iter(&cert)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| Error::Identity)?;
            let key = PrivateKeyDer::from_pem_slice(&key).map_err(|_| Error::Identity)?;
            config
                .with_client_auth_cert(chain, key)
                .map_err(|_| Error::Identity)?
        } else {
            config.with_no_client_auth()
        };
        options.set_transport(Transport::tls_with_config(config.into()));
        Ok(Self(options))
    }

    /// Plaintext local testing or a separately protected tunnel ending on loopback.
    pub fn loopback(client_id: &str, address: IpAddr, port: u16) -> Result<Self, Error> {
        if !address.is_loopback() {
            return Err(Error::Config);
        }
        Self::options(client_id, &address.to_string(), port).map(Self)
    }

    fn options(client_id: &str, host: &str, port: u16) -> Result<MqttOptions, Error> {
        if client_id.is_empty()
            || client_id.len() > u16::MAX.into()
            || client_id.contains('\0')
            || port == 0
        {
            return Err(Error::Config);
        }
        let mut options = MqttOptions::new(client_id, (host, port));
        options
            .set_keep_alive(15)
            .set_clean_start(true)
            .set_session_expiry_interval(Some(0))
            .set_receive_maximum(Some(32))
            .set_outgoing_inflight_upper_limit(32)
            .set_max_packet_size(Some(256 * 1024));
        Ok(options)
    }

    /// Add broker credentials for deployments using password authentication.
    pub fn credentials(mut self, username: String, password: String) -> Result<Self, Error> {
        if username.len() > u16::MAX.into()
            || password.len() > u16::MAX.into()
            || username.contains('\0')
        {
            return Err(Error::Config);
        }
        self.0.set_credentials(username, password);
        Ok(self)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid broker address, credential, client ID or trust settings")]
    Config,
    #[error("invalid CA trust anchors; supply PEM certificates")]
    TrustAnchors,
    #[error("invalid client identity; supply a PEM certificate chain and matching private key")]
    Identity,
    #[error("session is offline or waiting for subscription acknowledgement")]
    NotReady,
    #[error("outgoing queue is full")]
    Busy,
    #[error("broker connection failed; reconnect and reconcile outstanding commands")]
    Connection(#[source] Box<rumqttc::ConnectionError>),
    #[error("broker declined the QoS 1 subscription")]
    Subscription,
    #[error("broker rejected publication with packet ID {0} ({1:?})")]
    Publication(u16, PubAckReason),
    #[error("graceful MQTT disconnect timed out; pending outcomes are uncertain")]
    DrainTimeout,
    #[error("could not initialize the MQTT I/O runtime")]
    Runtime(#[source] std::io::Error),
    #[error("MQTT event loop closed")]
    Closed,
    #[error("system clock cannot supply a Unix timestamp")]
    Clock,
    #[error(transparent)]
    Route(#[from] RouteError),
    #[error(transparent)]
    Wire(#[from] WireError),
}

/// Events visible to the application. MQTT delivery is distinct from a CLS.EEDI ack.
#[derive(Debug)]
pub enum SessionEvent {
    Ready,
    Message {
        target: Target,
        message: Box<Message>,
    },
    /// A bounded, authorized route had an invalid payload. The wire error can
    /// carry diagnostic context. The binding queues applicable negative acknowledgements.
    Invalid {
        target: Target,
        error: WireError,
    },
    /// The broker accepted the publication but found no matching subscriptions.
    NoSubscribers {
        packet_id: u16,
    },
}

/// One MQTT client for a single provisioned route pair and role.
pub struct Session {
    broker: Broker,
    role: Role,
    routes: Routes,
    source: String,
    client: AsyncClient,
    events: BoxStream<'static, Result<Event, ConnectionError>>,
    runtime: tokio::runtime::Runtime,
    ready: bool,
    failed: bool,
}

impl Session {
    pub fn new(
        broker: Broker,
        role: Role,
        routes: Routes,
        source: impl Into<String>,
    ) -> Result<Self, Error> {
        let source = source.into();
        let mut options = broker.0.clone();
        if role == Role::LocalDevice {
            let topic = routes.topic(role, &Target::Presence)?;
            let offline = status(&source, None).encode()?;
            options.set_last_will(LastWill::new(topic, offline, QoS::AtLeastOnce, true, None));
        }
        let (client, event_loop) = AsyncClient::builder(options)
            .capacity(64)
            .try_build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            broker,
            role,
            routes,
            source,
            client,
            events: event_loop.into_stream().boxed(),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(Error::Runtime)?,
            ready: false,
            failed: false,
        })
    }

    /// Start a clean connection and provision a new last-will ID. Prior queued
    /// requests have uncertain outcomes. Reconcile them through the role engine.
    pub fn reconnect(&mut self) -> Result<(), Error> {
        *self = Self::new(
            self.broker.clone(),
            self.role,
            self.routes.clone(),
            self.source.clone(),
        )?;
        Ok(())
    }

    /// Drain queued traffic within `timeout` and send MQTT DISCONNECT. A local
    /// device asks the broker to publish its retained offline will. Dropping a
    /// live session exercises the broker's unexpected-disconnect handling.
    pub fn disconnect(mut self, timeout: Duration) -> Result<(), Error> {
        if !self.ready || self.failed {
            return Err(Error::NotReady);
        }
        let reason = match self.role {
            Role::Backend => DisconnectReasonCode::NormalDisconnection,
            Role::LocalDevice => DisconnectReasonCode::DisconnectWithWillMessage,
        };
        let properties = DisconnectProperties {
            session_expiry_interval: None,
            reason_string: None,
            user_properties: Vec::new(),
            server_reference: None,
        };
        self.client
            .try_disconnect_with_properties(reason, properties)
            .map_err(|_| Error::Busy)?;
        let start = Instant::now();
        loop {
            let remaining = timeout
                .checked_sub(start.elapsed())
                .ok_or(Error::DrainTimeout)?;
            match self.next_event(remaining)? {
                Some(Event::Outgoing(Outgoing::Disconnect)) => return Ok(()),
                Some(Event::Incoming(Incoming::PubAck(ack)))
                    if !matches!(
                        ack.reason,
                        PubAckReason::Success | PubAckReason::NoMatchingSubscribers
                    ) =>
                {
                    return Err(Error::Publication(ack.pkid, ack.reason))
                }
                Some(_) => {}
                None => return Err(Error::DrainTimeout),
            }
        }
    }

    /// Enqueue an application message at QoS 1. Only presence is retained.
    pub fn publish(&self, target: &Target, message: &Message) -> Result<(), Error> {
        if !self.ready || self.failed {
            return Err(Error::NotReady);
        }
        self.enqueue(target, message)
    }

    fn enqueue(&self, target: &Target, message: &Message) -> Result<(), Error> {
        Routes::check_kind(self.role, target, message.payload.kind())?;
        let topic = self.routes.topic(self.role, target)?;
        self.client
            .try_publish(
                topic,
                message.encode()?,
                PublishOptions::at_least_once().retain(target == &Target::Presence),
            )
            .map_err(|_| Error::Busy)
    }

    // The stream owns the in-progress EventLoop::poll future. Timing out a single
    // next() wait preserves that future, including an outstanding TLS/CONNACK wait.
    fn next_event(&mut self, budget: Duration) -> Result<Option<Event>, Error> {
        let result = self
            .runtime
            .block_on(async { tokio::time::timeout(budget, self.events.next()).await });
        match result {
            Err(_) => Ok(None),
            Ok(Some(Ok(event))) => Ok(Some(event)),
            Ok(Some(Err(e))) => {
                self.failed = true;
                Err(Error::Connection(Box::new(e)))
            }
            Ok(None) => {
                self.failed = true;
                Err(Error::Closed)
            }
        }
    }

    /// Advance one MQTT event. `None` means a transport-only event or elapsed timeout.
    /// A later poll resumes an in-progress connection, including its TLS and MQTT
    /// handshakes. Route and wire rejections leave the connection usable.
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<SessionEvent>, Error> {
        if self.failed {
            return Err(Error::Closed);
        }
        let Some(event) = self.next_event(timeout)? else {
            return Ok(None);
        };
        match event {
            Event::Incoming(Incoming::ConnAck(_)) => {
                let path = format!("{}/#", self.routes.base(self.role.peer()));
                // Retain As Published preserves the live publisher's RETAIN bit
                // as well as retained messages delivered after subscription.
                let mut filter = SubscribeFilter::new(path, QoS::AtLeastOnce);
                filter.preserve_retain = true;
                self.client
                    .try_subscribe_many([filter])
                    .map_err(|_| Error::Busy)?;
                Ok(None)
            }
            Event::Incoming(Incoming::SubAck(ack)) => {
                if ack.return_codes != [SubscribeReasonCode::Success(QoS::AtLeastOnce)] {
                    self.failed = true;
                    return Err(Error::Subscription);
                }
                self.ready = true;
                if self.role == Role::LocalDevice {
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| Error::Clock)?
                        .as_secs()
                        .try_into()
                        .map_err(|_| Error::Clock)?;
                    self.publish(&Target::Presence, &status(&self.source, Some(timestamp)))?;
                }
                Ok(Some(SessionEvent::Ready))
            }
            Event::Incoming(Incoming::Publish(packet)) => {
                let topic =
                    std::str::from_utf8(&packet.topic).map_err(|_| RouteError::Unprovisioned)?;
                let target = self.routes.receive(self.role, topic, packet.retain)?;
                let message = match Message::decode_for_reply(&packet.payload) {
                    Ok(message) => message,
                    Err(failure) => {
                        if let Some(reply) = failure.reply {
                            Routes::check_kind(self.role.peer(), &target, reply.kind)?;
                            let ack = Message::new(
                                Uuid::new_v4().to_string(),
                                &self.source,
                                Payload::Ack(Ack {
                                    error_number: reply.error_number,
                                }),
                            )
                            .related_to(reply.id);
                            let reply_target = match &target {
                                Target::Presence => &Target::Local,
                                other => other,
                            };
                            self.enqueue(reply_target, &ack)?;
                        }
                        return Ok(Some(SessionEvent::Invalid {
                            target,
                            error: failure.error,
                        }));
                    }
                };
                Routes::check_kind(self.role.peer(), &target, message.payload.kind())?;
                Ok(Some(SessionEvent::Message {
                    target,
                    message: Box::new(message),
                }))
            }
            Event::Incoming(Incoming::PubAck(ack)) => match ack.reason {
                PubAckReason::Success => Ok(None),
                PubAckReason::NoMatchingSubscribers => Ok(Some(SessionEvent::NoSubscribers {
                    packet_id: ack.pkid,
                })),
                reason => Err(Error::Publication(ack.pkid, reason)),
            },
            _ => Ok(None),
        }
    }
}

fn status(source: &str, timestamp: Option<i64>) -> Message {
    Message::new(
        Uuid::new_v4().to_string(),
        source,
        Payload::LocalDeviceStatus(Status {
            connected: timestamp.is_some(),
            timestamp,
        }),
    )
}
