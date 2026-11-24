mod recovery;

use std::collections::BTreeMap;

use super::*;
use crate::wire::{Read, Status};

/// Result of a verified publication received by the backend.
pub enum BackendEvent {
    /// Publish this control in response to the local device's read.
    Reply(Box<Message>),
    /// A terminal outcome for a previously submitted operation.
    Acknowledged {
        request_id: String,
        error_number: u64,
    },
    /// The local observed-state cache changed.
    StateUpdated,
    /// Presence changed on the device's dedicated presence route.
    Presence(Status),
    /// A snapshot raced a newer observation in its scope, or every supplied update was stale.
    Stale,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Submitted {
    at: u64,
    #[serde(deserialize_with = "recovery::read_control")]
    control: Control,
}

// REQ-10/11 distinguish six independently replaceable settings even though
// outstanding command serialization is per top-level family.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Setting {
    ConsumptionLimit,
    ProductionLimit,
    ConsumptionFailsafe,
    ProductionFailsafe,
    Trust,
    Notify,
}

impl Setting {
    fn of(control: &Control) -> Result<Self, EngineError> {
        // Callers validate standalone controls first, which guarantees one family and direction.
        if let Some(power) = &control.limits {
            Ok(if power.consumption.is_some() {
                Self::ConsumptionLimit
            } else {
                Self::ProductionLimit
            })
        } else if let Some(power) = &control.failsafes {
            Ok(if power.consumption.is_some() {
                Self::ConsumptionFailsafe
            } else {
                Self::ProductionFailsafe
            })
        } else if control.trust.is_some() {
            Ok(Self::Trust)
        } else if control.notify.is_some() {
            Ok(Self::Notify)
        } else {
            Err(EngineError::Unexpected)
        }
    }
}

enum Pending {
    Control(Submitted),
    Read {
        parameters: Vec<String>,
        raced_at: Option<i64>,
    },
}

/// Backend role for one authorized route. Callers schedule timeouts via [Self::cancel].
pub struct Backend {
    route: String,
    source: String,
    capacity: usize,
    pending: BTreeMap<String, Pending>,
    observations: observations::Observations,
    confirmed: BTreeMap<Setting, Submitted>,
    origin: u64,
    offset: u64,
    last_tick: u64,
    last_utc: i64,
}

impl Backend {
    /// `capacity` bounds outstanding controls and cached meters. A separate slot
    /// permits one state read even while every control slot awaits reconciliation.
    pub fn new(route: impl Into<String>, source: impl Into<String>, capacity: usize) -> Self {
        Self {
            route: route.into(),
            source: source.into(),
            capacity,
            pending: BTreeMap::new(),
            observations: Default::default(),
            confirmed: BTreeMap::new(),
            origin: 0,
            offset: 0,
            last_tick: 0,
            last_utc: 0,
        }
    }

    /// Latest validated observations for this route. The aggregate timestamp is the
    /// greatest accepted message timestamp. Individual properties may come from earlier messages.
    pub fn state(&self) -> Option<&State> {
        self.observations.state.as_ref()
    }

    /// Device timestamp of the last accepted observation or authoritative clear of a wire field.
    /// For `measurements` this is the latest time across its inventory and individual sources.
    pub fn property_observed_at(&self, field: &str) -> Option<i64> {
        self.observations.property_time(field)
    }

    /// Device timestamp of the cached reading for a particular meter, in Unix seconds.
    pub fn meter_observed_at(&self, id: &str) -> Option<i64> {
        self.observations.meter_time(id)
    }

    /// Cancel a request after timeout. A later reply cannot update the cache through this ID.
    /// Request IDs must be unique across retries and restarts.
    pub fn cancel(&mut self, id: &str) -> bool {
        self.pending.remove(id).is_some()
    }

    /// Outstanding operations whose equipment outcome still needs confirmation.
    /// These entries survive checkpoint restore and are never republished by the engine.
    pub fn pending_controls(&self) -> impl Iterator<Item = (&str, &Control)> {
        self.pending.iter().filter_map(|(id, p)| match p {
            Pending::Control(submitted) => Some((id.as_str(), &submitted.control)),
            Pending::Read { .. } => None,
        })
    }

    /// Confirmed desired settings, including remaining or expired lifetimes. Both power
    /// directions can be represented in memory even where the 1.2.0 reply cannot carry both.
    pub fn confirmed_settings(&mut self, now: Clock) -> Result<Control, EngineError> {
        let elapsed = self.clock(now)?;
        let mut cumulative = Control::default();
        let mut deadlines = Deadlines::default();
        for submitted in self.confirmed.values() {
            let request = &submitted.control;
            deadlines.update(request, submitted.at)?;
            merge_power(&mut cumulative.limits, &request.limits);
            merge_power(&mut cumulative.failsafes, &request.failsafes);
            if request.trust.is_some() {
                cumulative.trust.clone_from(&request.trust);
            }
            if request.notify.is_some() {
                cumulative.notify.clone_from(&request.notify);
            }
        }
        deadlines.apply(&mut cumulative.limits, elapsed);
        Ok(cumulative)
    }

    fn clock(&mut self, now: Clock) -> Result<u64, EngineError> {
        let tick = now
            .elapsed_seconds
            .checked_sub(self.origin)
            .and_then(|t| self.offset.checked_add(t))
            .ok_or(EngineError::Clock)?;
        if tick < self.last_tick || now.unix_seconds < self.last_utc {
            return Err(EngineError::Clock);
        }
        self.last_tick = tick;
        self.last_utc = now.unix_seconds;
        Ok(tick)
    }

    /// Register a standalone control before transmitting. Only one per family may be outstanding.
    pub fn send_control(
        &mut self,
        id: &str,
        request: Control,
        now: Clock,
    ) -> Result<Message, EngineError> {
        if self.pending.contains_key(id) {
            return Err(EngineError::IdConflict);
        }
        let received_at = self.clock(now)?;
        if self
            .pending_controls()
            .any(|(_, other)| conflicts(other, &request))
        {
            return Err(EngineError::Busy);
        }
        if self.pending_controls().count() >= self.capacity {
            return Err(EngineError::Capacity);
        }
        Deadlines::default().update(&request, received_at)?;
        let message = checked(Message::new(
            id,
            &self.source,
            Payload::Control(request.clone()),
        ))?;
        self.pending.insert(
            id.to_owned(),
            Pending::Control(Submitted {
                control: request,
                at: received_at,
            }),
        );
        Ok(message)
    }

    /// Register a state read. Empty parameters request a full snapshot.
    pub fn read(&mut self, id: &str, parameters: Vec<String>) -> Result<Message, EngineError> {
        if self.pending.contains_key(id) {
            return Err(EngineError::IdConflict);
        }
        if self.capacity == 0 {
            return Err(EngineError::Capacity);
        }
        if self
            .pending
            .values()
            .any(|p| matches!(p, Pending::Read { .. }))
        {
            return Err(EngineError::Busy);
        }
        let message = checked(Message::new(
            id,
            &self.source,
            Payload::Read(Read {
                parameters: parameters.clone(),
            }),
        ))?;
        self.pending.insert(
            id.to_owned(),
            Pending::Read {
                parameters,
                raced_at: None,
            },
        );
        Ok(message)
    }

    /// Consume a verified message received on the provisioned target. `reply_id` is used on replies.
    pub fn receive(
        &mut self,
        route: &str,
        message: &Message,
        reply_id: &str,
        now: Clock,
    ) -> Result<BackendEvent, EngineError> {
        if route != self.route {
            return Err(EngineError::Route);
        }
        message.validate()?;
        self.clock(now)?;
        match &message.payload {
            Payload::State(state) => self.receive_state(message.relation.as_deref(), state),
            Payload::LocalDeviceStatus(status) if message.relation.is_none() => {
                Ok(BackendEvent::Presence(status.clone()))
            }
            Payload::Ack(result) => {
                let id = message.relation.as_deref().unwrap_or(&message.id);
                let pending = self.pending.get(id).ok_or(EngineError::Unexpected)?;
                if result.error_number == 0 {
                    let Pending::Control(submitted) = pending else {
                        return Err(EngineError::Unexpected);
                    };
                    self.confirmed
                        .insert(Setting::of(&submitted.control)?, submitted.clone());
                }
                self.pending.remove(id);
                Ok(BackendEvent::Acknowledged {
                    request_id: id.to_owned(),
                    error_number: result.error_number,
                })
            }
            Payload::Read(read) if message.relation.is_none() && read.parameters.is_empty() => {
                if self.pending_controls().next().is_some() {
                    return Err(EngineError::ReconciliationRequired);
                }
                let desired = self.confirmed_settings(now)?;
                let reply = Message::new(reply_id, &self.source, Payload::Control(desired))
                    .related_to(&message.id);
                let reply = checked(reply).map_err(|_| EngineError::DesiredState)?;
                Ok(BackendEvent::Reply(Box::new(reply)))
            }
            _ => Err(EngineError::Unexpected),
        }
    }

    fn receive_state(
        &mut self,
        relation: Option<&str>,
        incoming: &State,
    ) -> Result<BackendEvent, EngineError> {
        let read = relation
            .map(|id| match self.pending.get(id) {
                Some(Pending::Read {
                    parameters,
                    raced_at,
                }) => Ok((parameters.as_slice(), *raced_at)),
                _ => Err(EngineError::Unexpected),
            })
            .transpose()?;
        let changed = self
            .observations
            .apply(incoming, read, self.capacity, &self.source)?;
        if let Some(id) = relation {
            self.pending.remove(id);
        } else if let Some(fields) = &changed {
            for pending in self.pending.values_mut() {
                if let Pending::Read {
                    parameters,
                    raced_at,
                } = pending
                {
                    if fields
                        .iter()
                        .any(|key| parameters.is_empty() || parameters.iter().any(|s| s == key))
                    {
                        *raced_at = Some(raced_at.unwrap_or(0).max(incoming.timestamp));
                    }
                }
            }
        }
        Ok(if changed.is_some() {
            BackendEvent::StateUpdated
        } else {
            BackendEvent::Stale
        })
    }
}
