mod commissioning;
mod recovery;

pub use commissioning::InstalledCertificate;

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use super::*;
use crate::wire::{Meter, Read, TrustEntry};

/// Effects produced by the local role.
pub enum DeviceEvent {
    /// Checkpoint this pending ID and obtain the command exactly once with
    /// [LocalDevice::control_for_dispatch] immediately before the equipment write.
    Apply { request_id: String },
    /// Publish this protocol response.
    Reply(Box<Message>),
    /// Pending duplicate, related control completion or duplicate without a wire response.
    Silent,
}

/// Definitive or uncertain outcome returned by the equipment adapter.
pub enum Outcome {
    /// All requested changes were accepted. Certificate trust needs [Self::Trusted].
    Accepted,
    /// Equipment accepted the entire list and identified every installed DER certificate.
    Trusted(Vec<InstalledCertificate>),
    /// The requested operation failed.
    Rejected,
    /// The target is disconnected or lacks the requested functionality.
    Unsupported,
    /// Keep the operation pending until the caller reconciles the equipment state.
    Unknown,
}

#[derive(Serialize, Deserialize)]
enum Execution {
    Pending {
        command: Control,
        // Never restore permission for another equipment write from disk.
        #[serde(skip)]
        reconcile: bool,
    },
    Done(u64),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    fingerprint: [u8; 32],
    related: bool,
    received: u64,
    finished: Option<u64>,
    execution: Execution,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Provisioning {
    route: String,
    source: String,
    subsequent: bool,
    capacity: usize,
    window: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: u8,
    provisioning: Provisioning,
    #[serde(deserialize_with = "recovery::read_ledger")]
    ledger: BTreeMap<String, Entry>,
    state: State,
    trust_revision: u64,
    deadlines: Deadlines,
    next_notification: Option<u64>,
    last_tick: u64,
    last_utc: i64,
}

/// Device state for one authorized route.
///
/// Completed operations are retained for at least the configured replay window.
/// Size the window for broker message expiry and the equipment's retry policy.
pub struct LocalDevice {
    journal: Checkpoint,
    awaiting_control: Option<String>,
    origin: u64,
    offset: u64,
}

impl LocalDevice {
    /// Create an engine with a 24-hour replay window and a hard entry/meter capacity.
    pub fn new(
        route: impl Into<String>,
        source: impl Into<String>,
        subsequent: bool,
        capacity: usize,
    ) -> Self {
        Self {
            journal: Checkpoint {
                version: 4,
                provisioning: Provisioning {
                    route: route.into(),
                    source: source.into(),
                    subsequent,
                    capacity,
                    window: 86_400,
                },
                ledger: BTreeMap::new(),
                state: State::default(),
                trust_revision: 0,
                deadlines: Deadlines::default(),
                next_notification: None,
                last_tick: 0,
                last_utc: 0,
            },
            awaiting_control: None,
            origin: 0,
            offset: 0,
        }
    }

    /// Set the minimum retention for completed IDs. Configure before admitting any operation.
    pub fn with_replay_window(mut self, seconds: u64) -> Result<Self, EngineError> {
        if seconds == 0 || !self.journal.ledger.is_empty() {
            return Err(EngineError::Unexpected);
        }
        self.journal.provisioning.window = seconds;
        Ok(self)
    }

    fn tick(&mut self, now: Clock) -> Result<u64, EngineError> {
        let tick = now
            .elapsed_seconds
            .checked_sub(self.origin)
            .and_then(|t| self.offset.checked_add(t))
            .ok_or(EngineError::Clock)?;
        if tick < self.journal.last_tick || now.unix_seconds < self.journal.last_utc {
            return Err(EngineError::Clock);
        }
        self.journal.last_tick = tick;
        self.journal.last_utc = now.unix_seconds;
        self.journal.ledger.retain(|_, e| {
            !e.finished.is_some_and(|finished| {
                tick.saturating_sub(finished) >= self.journal.provisioning.window
            })
        });
        Ok(tick)
    }

    /// Inspect operations requiring equipment reconciliation, including after restart.
    pub fn pending(&self) -> impl Iterator<Item = (&str, &Control)> {
        self.journal
            .ledger
            .iter()
            .filter_map(|(id, e)| match &e.execution {
                Execution::Pending { command, .. } => Some((id.as_str(), command)),
                Execution::Done(_) => None,
            })
    }

    /// Recalculate a queued operation immediately before its **first** equipment write.
    /// A timed activation or deactivation which expired in the queue returns `None`;
    /// complete it as rejected.
    /// Restored pending operations require independent reconciliation before any reissue.
    pub fn control_for_dispatch(
        &mut self,
        id: &str,
        now: Clock,
    ) -> Result<Option<Control>, EngineError> {
        let elapsed = self.tick(now)?;
        let entry = self
            .journal
            .ledger
            .get_mut(id)
            .ok_or(EngineError::Unexpected)?;
        let Execution::Pending { command, reconcile } = &mut entry.execution else {
            return Err(EngineError::Unexpected);
        };
        if *reconcile {
            return Err(EngineError::ReconciliationRequired);
        }
        let mut deadlines = Deadlines::default();
        deadlines.update(command, entry.received)?;
        if deadlines.0.into_iter().flatten().any(|end| end <= elapsed) {
            return Ok(None);
        }
        let mut command = command.clone();
        deadlines.apply(&mut command.limits, elapsed);
        *reconcile = true;
        Ok(Some(command))
    }

    /// Request the backend's accumulated control. Only one read can be outstanding.
    pub fn read_control(&mut self, id: &str) -> Result<Message, EngineError> {
        if self.awaiting_control.is_some() {
            return Err(EngineError::Busy);
        }
        let message = checked(Message::new(
            id,
            &self.journal.provisioning.source,
            Payload::Read(Read { parameters: vec![] }),
        ))?;
        self.awaiting_control = Some(id.to_owned());
        Ok(message)
    }

    /// Cancel the outstanding local read on timeout.
    pub fn cancel_read(&mut self) {
        self.awaiting_control = None;
    }

    /// Update locally discovered capabilities and return an immediate state advertisement.
    pub fn capabilities(
        &mut self,
        id: &str,
        values: Vec<String>,
        now: Clock,
    ) -> Result<Message, EngineError> {
        self.tick(now)?;
        let values = values
            .into_iter()
            .filter(|s| matches!(s.as_str(), "lpc" | "lpp" | "mgcp" | "mpc"))
            .collect::<Vec<_>>();
        let update = State {
            timestamp: now.unix_seconds,
            supported_eebus_use_cases: Some(values.clone()),
            ..State::default()
        };
        let message = checked(Message::new(
            id,
            &self.journal.provisioning.source,
            Payload::State(update),
        ))?;
        self.update_observations(|s| s.supported_eebus_use_cases = Some(values))?;
        Ok(message)
    }

    /// Replace observations with the adapter's complete source inventory. Omit lost sources
    /// and invalid readings. Readback then reflects the current inventory immediately.
    pub fn observe_meters(&mut self, meters: Vec<Meter>) -> Result<(), EngineError> {
        if meters.len() > self.journal.provisioning.capacity {
            return Err(EngineError::Capacity);
        }
        let mut ids = std::collections::BTreeSet::new();
        if !meters.iter().all(|m| ids.insert(&m.id)) {
            return Err(EngineError::Unexpected);
        }
        let meters = meters
            .into_iter()
            .filter(|m| matches!(m.source.as_str(), "gcp" | "controllable"))
            .collect();
        self.update_observations(|s| s.measurements = Some(meters))
    }

    // A full snapshot must still fit when independently admitted observations are combined.
    fn update_observations(&mut self, update: impl FnOnce(&mut State)) -> Result<(), EngineError> {
        let mut state = self.journal.state.clone();
        update(&mut state);
        let message = checked(Message::new(
            "validate",
            &self.journal.provisioning.source,
            Payload::State(state),
        ))?;
        if let Payload::State(state) = message.payload {
            self.journal.state = state;
        }
        Ok(())
    }

    /// Build current state at the supplied time without emitting a notification.
    pub fn snapshot(&mut self, now: Clock) -> Result<State, EngineError> {
        let elapsed = self.tick(now)?;
        let mut state = self.journal.state.clone();
        self.journal.deadlines.apply(&mut state.limits, elapsed);
        state.timestamp = now.unix_seconds;
        Ok(state)
    }

    /// Poll an active notification schedule. A late poll coalesces missed intervals.
    pub fn notification(&mut self, id: &str, now: Clock) -> Result<Option<Message>, EngineError> {
        let elapsed = self.tick(now)?;
        let Some(notify) = &self.journal.state.notify else {
            return Ok(None);
        };
        let (Some(interval), Some(end), Some(sources)) =
            (notify.interval, notify.end_time, &notify.source)
        else {
            return Ok(None);
        };
        if now.unix_seconds >= end {
            self.journal.next_notification = None;
            return Ok(None);
        }
        if !self.journal.next_notification.is_some_and(|t| t <= elapsed) {
            return Ok(None);
        }
        let meters = self
            .journal
            .state
            .measurements
            .iter()
            .flatten()
            .filter(|m| sources.contains(&m.source))
            .cloned()
            .collect();
        let valid_source = sources
            .iter()
            .any(|s| matches!(s.as_str(), "gcp" | "controllable"));
        let result = if valid_source {
            Some(checked(Message::new(
                id,
                &self.journal.provisioning.source,
                Payload::State(State {
                    measurements: Some(meters),
                    timestamp: now.unix_seconds,
                    ..State::default()
                }),
            ))?)
        } else {
            None
        };
        self.journal.next_notification = elapsed.checked_add(u64::from(interval));
        Ok(result)
    }

    /// Process a validated publication for this route. Equipment must only act on [DeviceEvent::Apply].
    pub fn receive(
        &mut self,
        route: &str,
        message: &Message,
        reply_id: &str,
        now: Clock,
    ) -> Result<DeviceEvent, EngineError> {
        if route != self.journal.provisioning.route {
            return Err(EngineError::Route);
        }
        message.validate()?;
        match &message.payload {
            Payload::Control(control) => self.receive_control(message, control, now),
            Payload::Read(read) if message.relation.is_none() => {
                if self.journal.provisioning.subsequent
                    && read.parameters.iter().any(|s| s == "trust")
                {
                    return Ok(DeviceEvent::Reply(Box::new(ack(
                        &self.journal.provisioning.source,
                        &message.id,
                        2,
                    )?)));
                }
                let state = filter_state(self.snapshot(now)?, &read.parameters);
                let response = Message::new(
                    reply_id,
                    &self.journal.provisioning.source,
                    Payload::State(state),
                )
                .related_to(&message.id);
                Ok(DeviceEvent::Reply(Box::new(checked(response)?)))
            }
            Payload::Ack(result)
                if self.awaiting_control.as_deref()
                    == Some(message.relation.as_deref().unwrap_or(&message.id))
                    && result.error_number > 0 =>
            {
                self.awaiting_control = None;
                Ok(DeviceEvent::Silent)
            }
            _ => Err(EngineError::Unexpected),
        }
    }

    fn supports(&self, c: &Control) -> bool {
        let has = |name| {
            self.journal
                .state
                .supported_eebus_use_cases
                .iter()
                .flatten()
                .any(|s| s == name)
        };
        let consumption = c.limits.as_ref().is_some_and(|p| p.consumption.is_some())
            || c.failsafes
                .as_ref()
                .is_some_and(|p| p.consumption.is_some());
        let production = c.limits.as_ref().is_some_and(|p| p.production.is_some())
            || c.failsafes.as_ref().is_some_and(|p| p.production.is_some());
        (!consumption || has("lpc")) && (!production || has("lpp"))
    }

    fn receive_control(
        &mut self,
        message: &Message,
        control: &Control,
        now: Clock,
    ) -> Result<DeviceEvent, EngineError> {
        let elapsed = self.tick(now)?;
        let fingerprint: [u8; 32] = Sha256::digest(message.encode()?).into();
        let related = message.relation.is_some();
        if let Some(prior) = self.journal.ledger.get(&message.id) {
            if fingerprint != prior.fingerprint {
                return Err(EngineError::IdConflict);
            }
            return match prior.execution {
                Execution::Done(code) if !related => Ok(DeviceEvent::Reply(Box::new(ack(
                    &self.journal.provisioning.source,
                    &message.id,
                    code,
                )?))),
                _ => Ok(DeviceEvent::Silent),
            };
        }
        if related && message.relation != self.awaiting_control {
            return Err(EngineError::Unexpected);
        }
        // REQ-60 is independent of equipment and replay-ledger capacity. REQ-27
        // suppresses an ACK when this is the backend's reply to our own read.
        if control.trust.is_some() && self.journal.provisioning.subsequent {
            if related {
                self.awaiting_control = None;
                return Ok(DeviceEvent::Silent);
            }
            return Ok(DeviceEvent::Reply(Box::new(ack(
                &self.journal.provisioning.source,
                &message.id,
                2,
            )?)));
        }
        if self.journal.ledger.len() >= self.journal.provisioning.capacity {
            return Err(EngineError::Capacity);
        }
        if !related {
            // Reserve a representable terminal acknowledgement before exposing an equipment write.
            ack(&self.journal.provisioning.source, &message.id, 0)?;
        }
        let mut deadlines = Deadlines::default();
        deadlines.update(control, elapsed)?;
        let error = if !self.supports(control) {
            Some(4)
        } else if self.pending().any(|(_, other)| conflicts(control, other)) {
            Some(3)
        } else if !families(control).contains(&true) {
            Some(0)
        } else {
            None
        };
        let event = match error {
            Some(code) if !related => DeviceEvent::Reply(Box::new(ack(
                &self.journal.provisioning.source,
                &message.id,
                code,
            )?)),
            Some(_) => DeviceEvent::Silent,
            None => DeviceEvent::Apply {
                request_id: message.id.clone(),
            },
        };
        self.journal.ledger.insert(
            message.id.clone(),
            Entry {
                fingerprint,
                related,
                received: elapsed,
                finished: error.map(|_| elapsed),
                execution: error.map_or_else(
                    || Execution::Pending {
                        command: control.clone(),
                        reconcile: false,
                    },
                    Execution::Done,
                ),
            },
        );
        if related {
            self.awaiting_control = None;
        }
        Ok(event)
    }

    /// Record downstream acceptance or failure. Unknown results stay pending. If acceptance
    /// times out, reconcile equipment before calling this again. Related controls have no ack.
    pub fn complete(
        &mut self,
        request_id: &str,
        outcome: Outcome,
        now: Clock,
    ) -> Result<Option<Message>, EngineError> {
        let elapsed = self.tick(now)?;
        let entry = self
            .journal
            .ledger
            .get(request_id)
            .ok_or(EngineError::Unexpected)?;
        let Execution::Pending { command: c, .. } = &entry.execution else {
            return Err(EngineError::Unexpected);
        };
        let (code, trust) = match outcome {
            Outcome::Unknown => {
                if let Some(Entry {
                    execution: Execution::Pending { reconcile, .. },
                    ..
                }) = self.journal.ledger.get_mut(request_id)
                {
                    *reconcile = true;
                }
                return Ok(None);
            }
            Outcome::Rejected => (3, None),
            Outcome::Unsupported => (4, None),
            Outcome::Trusted(reports) => {
                let requested = c.trust.as_deref().ok_or(EngineError::Unexpected)?;
                (0, Some(commissioning::confirmed_trust(requested, reports)?))
            }
            Outcome::Accepted if c.trust.iter().flatten().any(|t| t.certificate.is_some()) => {
                return Err(EngineError::TrustReadback)
            }
            Outcome::Accepted => (0, c.trust.clone()),
        };
        let reply = if entry.related {
            None
        } else {
            Some(ack(&self.journal.provisioning.source, request_id, code)?)
        };
        if code == 0 {
            let mut state = self.journal.state.clone();
            merge_power(&mut state.limits, &c.limits);
            merge_power(&mut state.failsafes, &c.failsafes);
            let revision = if trust.is_some() {
                state.trust = trust;
                self.journal
                    .trust_revision
                    .checked_add(1)
                    .ok_or(EngineError::Capacity)?
            } else {
                self.journal.trust_revision
            };
            if c.notify.is_some() {
                state.notify.clone_from(&c.notify);
            }
            checked(Message::new(
                "validate",
                &self.journal.provisioning.source,
                Payload::State(state.clone()),
            ))?;
            self.journal.deadlines.update(c, entry.received)?;
            if let Some(notify) = &c.notify {
                self.journal.next_notification = notify
                    .interval
                    .and_then(|i| elapsed.checked_add(u64::from(i)));
            }
            self.journal.state = state;
            self.journal.trust_revision = revision;
        }
        let entry = self
            .journal
            .ledger
            .get_mut(request_id)
            .expect("entry was just inspected");
        entry.execution = Execution::Done(code);
        entry.finished = Some(elapsed);
        Ok(reply)
    }
}
