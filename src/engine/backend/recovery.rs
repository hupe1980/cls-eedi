//! Backend recovery stores confirmed settings and outstanding control requests.
//! Observations and read requests belong to the prior connection and must be refreshed.
use super::*;
use serde::{de::Error as _, Deserializer};
use serde_json::value::RawValue;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u8,
    route: String,
    source: String,
    capacity: usize,
    tick: u64,
    utc: i64,
    #[serde(deserialize_with = "bounded::<_, _, 6>")]
    confirmed: Vec<Submitted>,
    #[serde(deserialize_with = "bounded::<_, _, 4>")]
    outstanding: Vec<(String, Submitted)>,
}

// Limit the number of decoded entries before building an attacker-sized journal model.
fn bounded<'de, D, T, const MAX: usize>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    use serde::de::{SeqAccess, Visitor};
    struct Items<T, const MAX: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for Items<T, MAX> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {MAX} journal entries")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut out = Vec::new();
            while let Some(item) = seq.next_element()? {
                if out.len() == MAX {
                    return Err(A::Error::custom("too many journal entries"));
                }
                out.push(item);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(Items::<T, MAX>(std::marker::PhantomData))
}

// Network controls accept unknown extensions; stored, versioned controls must round trip
// without dropping or changing fields. Typed deserialization rejects duplicate known keys.
pub(super) fn read_control<'de, D: Deserializer<'de>>(d: D) -> Result<Control, D::Error> {
    let raw = Box::<RawValue>::deserialize(d)?;
    if raw.get().len() > crate::wire::MAX_MESSAGE_BYTES {
        return Err(D::Error::custom("saved control too large"));
    }
    let control: Control = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
    let original: serde_json::Value = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
    if original != serde_json::to_value(&control).map_err(D::Error::custom)? {
        return Err(D::Error::custom("invalid saved control shape"));
    }
    Ok(control)
}

impl Backend {
    fn journal_byte_limit(&self) -> usize {
        // Six replaceable settings, up to four pending property families and framing.
        // JSON may escape each byte of a deployment-provided route or source.
        let messages = (7 + self.capacity.min(4)) * crate::wire::MAX_MESSAGE_BYTES;
        let identity = self
            .route
            .len()
            .saturating_add(self.source.len())
            .saturating_mul(6);
        messages.saturating_add(identity)
    }

    /// Checkpoint before publishing a newly registered control and again after its ACK.
    /// Store these potentially credential-bearing bytes atomically under exclusive route ownership.
    pub fn checkpoint(&mut self, now: Clock) -> Result<Vec<u8>, EngineError> {
        let tick = self.clock(now)?;
        let outstanding = self
            .pending
            .iter()
            .filter_map(|(id, pending)| match pending {
                Pending::Control(submitted) => Some((id.clone(), submitted.clone())),
                Pending::Read { .. } => None,
            })
            .collect();
        let journal = Journal {
            version: 1,
            route: self.route.clone(),
            source: self.source.clone(),
            capacity: self.capacity,
            tick,
            utc: now.unix_seconds,
            confirmed: self.confirmed.values().cloned().collect(),
            outstanding,
        };
        let bytes = serde_json::to_vec(&journal).map_err(|_| EngineError::Checkpoint)?;
        if bytes.len() > self.journal_byte_limit() {
            return Err(EngineError::Capacity);
        }
        Ok(bytes)
    }

    /// Recover using externally provisioned route, source and capacity. Restored commands
    /// await the original ACK. A state read can reconcile uncertain equipment while conflicting
    /// writes and desired-control replies stay blocked. Explicit cancellation abandons correlation.
    pub fn restore(mut self, bytes: &[u8], now: Clock) -> Result<Self, EngineError> {
        if bytes.len() > self.journal_byte_limit() {
            return Err(EngineError::Checkpoint);
        }
        let journal: Journal =
            serde_json::from_slice(bytes).map_err(|_| EngineError::Checkpoint)?;
        if journal.version != 1
            || journal.utc < 0
            || journal.route != self.route
            || journal.source != self.source
            || journal.capacity != self.capacity
            || journal.outstanding.len() > self.capacity
            || (self.capacity == 0 && !journal.confirmed.is_empty())
        {
            return Err(EngineError::Checkpoint);
        }
        let downtime = now
            .unix_seconds
            .checked_sub(journal.utc)
            .and_then(|t| u64::try_from(t).ok())
            .ok_or(EngineError::Clock)?;
        let resumed = journal
            .tick
            .checked_add(downtime)
            .ok_or(EngineError::Clock)?;
        let validate = |id: &str, item: &Submitted| -> Result<(), EngineError> {
            if item.at > journal.tick {
                return Err(EngineError::Checkpoint);
            }
            Message::new(id, &self.source, Payload::Control(item.control.clone()))
                .encode()
                .map_err(|_| EngineError::Checkpoint)?;
            Deadlines::default()
                .update(&item.control, item.at)
                .map_err(|_| EngineError::Checkpoint)
        };
        let mut confirmed = BTreeMap::new();
        for item in journal.confirmed {
            validate("", &item)?;
            let setting = Setting::of(&item.control).map_err(|_| EngineError::Checkpoint)?;
            if confirmed.insert(setting, item).is_some() {
                return Err(EngineError::Checkpoint);
            }
        }
        let mut pending = BTreeMap::new();
        let mut occupied = [false; 4];
        for (id, item) in journal.outstanding {
            validate(&id, &item)?;
            // Same-family controls are serialized. An older unresolved record could
            // otherwise replace a newer confirmed setting when its delayed ACK arrives.
            if confirmed
                .values()
                .any(|prior| prior.at > item.at && conflicts(&prior.control, &item.control))
            {
                return Err(EngineError::Checkpoint);
            }
            for (busy, requested) in occupied.iter_mut().zip(families(&item.control)) {
                if *busy && requested {
                    return Err(EngineError::Checkpoint);
                }
                *busy |= requested;
            }
            if pending.insert(id, Pending::Control(item)).is_some() {
                return Err(EngineError::Checkpoint);
            }
        }
        self.origin = now.elapsed_seconds;
        self.offset = resumed;
        self.last_tick = resumed;
        self.last_utc = now.unix_seconds;
        self.pending = pending;
        self.confirmed = confirmed;
        self.observations = Default::default();
        Ok(self)
    }
}
