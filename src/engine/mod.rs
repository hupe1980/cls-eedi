//! Deterministic, per-route CLS.EEDI roles. Callers own clocks, MQTT and equipment.
//!
//! Keep one engine per authorized target. Checkpoint a local device before issuing
//! its requested equipment operation and again before publishing its completion.
//! On restart, reconcile pending IDs with equipment. A lost acknowledgement can
//! then be replayed without executing the operation again.

mod backend;
mod device;
mod observations;

pub use backend::{Backend, BackendEvent};
pub use device::{DeviceEvent, InstalledCertificate, LocalDevice, Outcome};
use serde::{Deserialize, Serialize};

use crate::wire::{Control, Message, Payload, Power, State, WireError};

/// Synchronized UTC and monotonic seconds sampled together by the application.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Clock {
    /// Seconds since the Unix epoch.
    pub unix_seconds: i64,
    /// Seconds from an arbitrary process-local monotonic origin.
    pub elapsed_seconds: u64,
}

impl Clock {
    /// Construct a clock sample. The engine rejects negative UTC or regressing elapsed time.
    pub const fn new(unix_seconds: i64, elapsed_seconds: u64) -> Self {
        Self {
            unix_seconds,
            elapsed_seconds,
        }
    }
}

/// Application or protocol interaction errors. Display never contains payload data.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The supplied route differs from the provisioned target.
    #[error("publication does not match the engine route")]
    Route,
    /// The peer sent an unexpected message or correlated it to a different exchange.
    #[error("unexpected message type or relation")]
    Unexpected,
    /// Another operation for this property, or another read, is in flight.
    #[error("a conflicting request is already in flight")]
    Busy,
    /// Capacity is exhausted. No existing pending operation was evicted.
    #[error("engine capacity exhausted")]
    Capacity,
    /// Repeated IDs with different content cannot share a completion.
    #[error("request ID reused with different content")]
    IdConflict,
    /// Equipment may already have acted. Reconcile its observed outcome before completion.
    #[error("equipment outcome requires reconciliation")]
    ReconciliationRequired,
    /// The caller must supply synchronized UTC and a monotonic elapsed clock.
    #[error("clock regressed or deadline cannot be represented")]
    Clock,
    /// Stored state is damaged, from an unsupported revision or for a different route.
    #[error("invalid device checkpoint")]
    Checkpoint,
    /// Trust readback must contain the certificate's SKI supplied by the equipment.
    #[error("a verified trust readback is required")]
    TrustReadback,
    /// A pairing observation is stale, ambiguous or inconsistent with the active credential.
    #[error("pairing result does not match the active trust replacement")]
    PairingMismatch,
    /// A multi-direction desired state has no unambiguous 1.2.0 control encoding.
    #[error("accumulated settings cannot be expressed in one legal control reply")]
    DesiredState,
    /// The message violates the wire contract.
    #[error(transparent)]
    Wire(#[from] WireError),
}

fn checked(message: Message) -> Result<Message, EngineError> {
    message.encode()?;
    Ok(message)
}

fn families(c: &Control) -> [bool; 4] {
    [
        c.limits.is_some(),
        c.failsafes.is_some(),
        c.trust.is_some(),
        c.notify.is_some(),
    ]
}

fn conflicts(a: &Control, b: &Control) -> bool {
    families(a)
        .into_iter()
        .zip(families(b))
        .any(|(a, b)| a && b)
}

fn merge_power<T: Clone>(current: &mut Option<Power<T>>, patch: &Option<Power<T>>) {
    if let Some(patch) = patch {
        let slot = current.get_or_insert_with(|| Power {
            consumption: None,
            production: None,
        });
        if patch.consumption.is_some() {
            slot.consumption.clone_from(&patch.consumption);
        }
        if patch.production.is_some() {
            slot.production.clone_from(&patch.production);
        }
    }
}

/// Absolute deadlines in engine elapsed seconds, one per power direction.
#[derive(Default, Serialize, Deserialize)]
struct Deadlines([Option<u64>; 2]);

impl Deadlines {
    fn update(&mut self, c: &Control, received_at: u64) -> Result<(), EngineError> {
        if let Some(power) = &c.limits {
            for (slot, limit) in self
                .0
                .iter_mut()
                .zip([&power.consumption, &power.production])
            {
                if let Some(limit) = limit {
                    *slot = limit
                        .duration
                        .map(|d| {
                            received_at
                                .checked_add(u64::from(d))
                                .ok_or(EngineError::Clock)
                        })
                        .transpose()?;
                }
            }
        }
        Ok(())
    }

    fn apply(&self, power: &mut Option<Power<crate::wire::Limit>>, now: u64) {
        if let Some(power) = power {
            for (slot, limit) in self
                .0
                .iter()
                .zip([&mut power.consumption, &mut power.production])
            {
                if let (Some(expires), Some(limit)) = (slot, limit) {
                    let left = expires.saturating_sub(now);
                    if left == 0 {
                        limit.active = false;
                        limit.duration = None;
                    } else {
                        limit.duration = Some(u32::try_from(left).unwrap_or(u32::MAX));
                    }
                }
            }
        }
    }
}

const FIELDS: [&str; 6] = [
    "limits",
    "failsafes",
    "trust",
    "notify",
    "measurements",
    "supportedEebusUseCases",
];

fn filter_state(mut state: State, fields: &[String]) -> State {
    if !fields.is_empty() {
        for field in FIELDS {
            if !fields.iter().any(|f| f == field) {
                clear_field(&mut state, field);
            }
        }
    }
    state
}

fn clear_field(state: &mut State, field: &str) {
    match field {
        "limits" => state.limits = None,
        "failsafes" => state.failsafes = None,
        "trust" => state.trust = None,
        "notify" => state.notify = None,
        "measurements" => state.measurements = None,
        "supportedEebusUseCases" => state.supported_eebus_use_cases = None,
        _ => {}
    }
}

fn ack(source: &str, relation: &str, error_number: u64) -> Result<Message, EngineError> {
    checked(
        Message::new(
            relation,
            source,
            Payload::Ack(crate::wire::Ack { error_number }),
        )
        .related_to(relation),
    )
}
