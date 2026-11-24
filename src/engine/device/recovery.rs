//! Application-owned crash recovery records and invariants.
use super::*;

impl Provisioning {
    fn byte_limit(&self) -> usize {
        // A journal entry fits its original wire message plus metadata. Extra slots
        // cover provisioning, state and JSON framing.
        self.capacity
            .saturating_add(4)
            .saturating_mul(crate::wire::MAX_MESSAGE_BYTES + 2048)
    }
}

// A repeated ledger key would silently replace the earlier equipment outcome in a
// plain BTreeMap deserialization. Treat an ambiguous journal as damaged.
pub(super) fn read_ledger<'de, D>(input: D) -> Result<BTreeMap<String, Entry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, MapAccess, Visitor};
    struct Entries;
    impl<'de> Visitor<'de> for Entries {
        type Value = BTreeMap<String, Entry>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a journal with unique IDs")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries = BTreeMap::new();
            while let Some((id, entry)) = map.next_entry()? {
                if entries.insert(id, entry).is_some() {
                    return Err(A::Error::custom("duplicate journal ID"));
                }
            }
            Ok(entries)
        }
    }
    input.deserialize_map(Entries)
}

impl Checkpoint {
    fn validate(&self, expected: &Provisioning) -> Result<(), EngineError> {
        if self.version != 4
            || &self.provisioning != expected
            || self.ledger.len() > expected.capacity
            || self.last_utc < 0
            || self
                .state
                .measurements
                .as_ref()
                .is_some_and(|m| m.len() > expected.capacity)
            || (expected.subsequent && self.state.trust.is_some())
            || (self.trust_revision != 0) != self.state.trust.is_some()
        {
            return Err(EngineError::Checkpoint);
        }
        let validate = |message: Message| {
            message
                .encode()
                .map(|_| ())
                .map_err(|_| EngineError::Checkpoint)
        };
        validate(Message::new(
            "validate",
            &expected.source,
            Payload::State(self.state.clone()),
        ))?;
        for (end, limit) in self.deadlines.0.into_iter().zip([
            self.state
                .limits
                .as_ref()
                .and_then(|p| p.consumption.as_ref()),
            self.state
                .limits
                .as_ref()
                .and_then(|p| p.production.as_ref()),
        ]) {
            let duration = limit.and_then(|l| l.duration);
            let consistent = match (end, duration) {
                (None, None) => true,
                (Some(end), Some(duration)) => end
                    .checked_sub(u64::from(duration))
                    .is_some_and(|start| start <= self.last_tick),
                _ => false,
            };
            if !consistent {
                return Err(EngineError::Checkpoint);
            }
        }
        if let Some(next) = self.next_notification {
            let interval = self
                .state
                .notify
                .as_ref()
                .and_then(|n| n.interval)
                .ok_or(EngineError::Checkpoint)?;
            if next.saturating_sub(self.last_tick) > u64::from(interval) {
                return Err(EngineError::Checkpoint);
            }
        }

        let mut occupied = [false; 4];
        for (id, entry) in &self.ledger {
            if entry.received > self.last_tick {
                return Err(EngineError::Checkpoint);
            }
            match (&entry.execution, entry.finished) {
                (Execution::Done(0 | 2 | 3 | 4), Some(finished))
                    if (entry.received..=self.last_tick).contains(&finished) =>
                {
                    if !entry.related {
                        ack(&expected.source, id, 0).map_err(|_| EngineError::Checkpoint)?;
                    }
                }
                (Execution::Pending { command, .. }, None) => {
                    if expected.subsequent && command.trust.is_some() {
                        return Err(EngineError::Checkpoint);
                    }
                    let requested = families(command);
                    if !requested.contains(&true) {
                        return Err(EngineError::Checkpoint);
                    }
                    for (busy, family) in occupied.iter_mut().zip(requested) {
                        if *busy && family {
                            return Err(EngineError::Checkpoint);
                        }
                        *busy |= family;
                    }
                    let mut message = Message::new(id, "", Payload::Control(command.clone()));
                    if entry.related {
                        message = message.related_to("");
                    }
                    validate(message)?;
                    Deadlines::default()
                        .update(command, entry.received)
                        .map_err(|_| EngineError::Checkpoint)?;
                }
                _ => return Err(EngineError::Checkpoint),
            }
        }
        Ok(())
    }
}

impl LocalDevice {
    /// Create a point-in-time checkpoint. It can contain pairing secrets. Store atomically
    /// under application access controls, before actuation and before sending its acknowledgement.
    pub fn checkpoint(&mut self, now: Clock) -> Result<Vec<u8>, EngineError> {
        self.tick(now)?;
        let bytes = serde_json::to_vec(&self.journal).map_err(|_| EngineError::Checkpoint)?;
        if bytes.len() > self.journal.provisioning.byte_limit() {
            return Err(EngineError::Capacity);
        }
        Ok(bytes)
    }

    /// Resume from an application-controlled checkpoint using externally provisioned identity,
    /// target kind, capacity and replay window. Use a fresh engine and synchronized UTC.
    /// Commit checkpoints atomically with one writer per route and protect them against rollback.
    /// Recovered pending operations remain blocked from dispatch until equipment confirms their outcome.
    pub fn restore(mut self, bytes: &[u8], now: Clock) -> Result<Self, EngineError> {
        let byte_limit = self.journal.provisioning.byte_limit();
        if bytes.len() > byte_limit {
            return Err(EngineError::Checkpoint);
        }
        let mut journal: Checkpoint =
            serde_json::from_slice(bytes).map_err(|_| EngineError::Checkpoint)?;
        journal.validate(&self.journal.provisioning)?;
        let downtime = now
            .unix_seconds
            .checked_sub(journal.last_utc)
            .and_then(|t| u64::try_from(t).ok())
            .ok_or(EngineError::Clock)?;
        self.origin = now.elapsed_seconds;
        self.offset = journal
            .last_tick
            .checked_add(downtime)
            .ok_or(EngineError::Clock)?;
        for entry in journal.ledger.values_mut() {
            if let Execution::Pending { reconcile, .. } = &mut entry.execution {
                *reconcile = true;
            }
            // Downtime ages physical limits, but cannot alone authorize a repeated write.
            if let Some(finished) = &mut entry.finished {
                *finished = finished.checked_add(downtime).ok_or(EngineError::Clock)?;
            }
        }
        self.journal = journal;
        self.awaiting_control = None;
        self.tick(now)?;
        Ok(self)
    }
}
