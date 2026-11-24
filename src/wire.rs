//! Typed CLS.EEDI 1.x JSON messages. The default sender version is 1.2.0.
//!
//! Use [Message::decode] at the trust boundary. Its budgets include unknown extension
//! fields. Explicit nulls are rejected for typed fields and duplicate keys are rejected
//! throughout a message. Deserializing a payload struct directly skips these checks.

mod decimal;
mod object;
mod trust_report;
mod types;
mod validation;

pub use trust_report::{TrustReport, TrustSummary};
pub use types::*;
use validation::{enforce_collection_budget, known_source, require, validate_payload};

use semver::Version;
use serde::{
    de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use serde_json::{value::RawValue, Map, Value};
use std::fmt;

/// Maximum payload accepted by the default codec, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;

/// Wire resource budgets. The byte limit is checked before parsing.
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    pub bytes: usize,
    pub depth: usize,
    pub entries_per_container: usize,
}
impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            bytes: MAX_MESSAGE_BYTES,
            depth: 32,
            entries_per_container: 256,
        }
    }
}

#[derive(thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("message exceeds the configured byte limit")]
    TooLarge,
    #[error("invalid JSON, duplicate key, or container limit exceeded")]
    InvalidJson,
    #[error("invalid CLS.EEDI field: {0}")]
    Field(&'static str),
    #[error("CLS.EEDI protocol violation: {0}")]
    Protocol(&'static str),
    #[error("incompatible CLS.EEDI major version")]
    UnsupportedMajor { id: String, kind: Kind },
}
// No user-controlled value is interpolated into diagnostics.
impl fmt::Debug for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Correlation metadata for a protocol-defined error reply.
#[derive(Clone, PartialEq, Eq)]
pub struct ReplyHint {
    pub id: String,
    pub kind: Kind,
    pub error_number: u64,
}
impl fmt::Debug for ReplyHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplyHint")
            .field("kind", &self.kind)
            .field("error_number", &self.error_number)
            .finish_non_exhaustive()
    }
}
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct DecodeFailure {
    pub error: WireError,
    pub reply: Option<ReplyHint>,
}
impl From<WireError> for DecodeFailure {
    fn from(error: WireError) -> Self {
        Self { error, reply: None }
    }
}
impl WireError {
    fn ack_code(&self) -> u64 {
        match self {
            Self::UnsupportedMajor { .. } | Self::Protocol(_) => 2,
            _ => 1,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub source: String,
    pub relation: Option<String>,
    pub protocol: Version,
    pub payload: Payload,
}
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("kind", &self.payload.kind())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize, Serialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: Kind,
    id: String,
    #[serde(default)]
    source: Value,
    #[serde(default)]
    specversion: Value,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    relation: Option<String>,
}

impl Message {
    /// Builds an outbound 1.2.0 message. Validation occurs on encode.
    pub fn new(id: impl Into<String>, source: impl Into<String>, payload: Payload) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            protocol: Version::new(1, 2, 0),
            relation: None,
            payload,
        }
    }
    #[must_use]
    pub fn related_to(mut self, id: impl Into<String>) -> Self {
        self.relation = Some(id.into());
        self
    }
    /// Decodes with default resource budgets. An incompatible major returns the original
    /// ID and kind so the role can correlate a negative acknowledgement and suppress ack loops.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        Self::decode_with(bytes, DecodeLimits::default())
    }
    pub fn decode_with(bytes: &[u8], limits: DecodeLimits) -> Result<Self, WireError> {
        Self::decode_bounded(bytes, limits).map_err(|failure| failure.error)
    }
    /// Provides a possible negative acknowledgement only after resolving an unambiguous
    /// envelope. The caller must still authorize the route and check the message direction.
    /// Code 1 denotes invalid schema data and code 2 a protocol violation. Major-version
    /// incompatibility produces a hint for any recognized non-ack message (REQ-3/4).
    /// Within the supported major, reads and standalone controls are eligible (REQ-27/28).
    pub fn decode_for_reply(bytes: &[u8]) -> Result<Self, DecodeFailure> {
        Self::decode_bounded(bytes, DecodeLimits::default())
    }
    fn decode_bounded(bytes: &[u8], limits: DecodeLimits) -> Result<Self, DecodeFailure> {
        if bytes.len() > limits.bytes {
            return Err(WireError::TooLarge.into());
        }
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let root = deserializer
            .deserialize_map(CheckedJson {
                limits: &limits,
                depth: 0,
            })
            .map_err(|_| WireError::InvalidJson)?;
        deserializer.end().map_err(|_| WireError::InvalidJson)?;
        let Value::Object(mut fields) = root else {
            return Err(WireError::Field("envelope").into());
        };
        let data = fields.remove("data").unwrap_or(Value::Null);
        let mut envelope: Envelope = typed(Value::Object(fields), "envelope")?;
        Self::from_envelope(&mut envelope, data).map_err(|error| {
            let kind = envelope.kind;
            let eligible = match (&error, kind) {
                (_, Kind::Ack) => false,
                // REQ-3/4 decide compatibility failures before 1.x role semantics.
                (WireError::UnsupportedMajor { .. }, _) => true,
                (_, Kind::Read) => true,
                (_, Kind::Control) => envelope.relation.is_none(),
                _ => false,
            };
            let reply = eligible.then(|| ReplyHint {
                id: envelope.id,
                kind,
                error_number: error.ack_code(),
            });
            DecodeFailure { error, reply }
        })
    }
    fn from_envelope(envelope: &mut Envelope, data: Value) -> Result<Self, WireError> {
        let Value::String(source) = envelope.source.take() else {
            return Err(WireError::Field("source"));
        };
        match envelope.specversion.as_str() {
            Some("1.0") => {}
            Some(_) => return Err(WireError::Protocol("specversion")),
            None => return Err(WireError::Field("specversion")),
        }
        let Value::Object(mut data) = data else {
            return Err(WireError::Field("data"));
        };
        let protocol = parse_version(data.remove("protocol"))?;
        if protocol.major != 1 {
            return Err(WireError::UnsupportedMajor {
                id: envelope.id.clone(),
                kind: envelope.kind,
            });
        }
        // An unfamiliar metering source has no defined meaning even if it has new fields
        // whose shapes differ from the current profile. Ignore the entire entry (REQ-43).
        if envelope.kind == Kind::State {
            if let Some(meters) = data.get_mut("measurements").and_then(Value::as_array_mut) {
                meters.retain(|m| {
                    m.get("source")
                        .and_then(Value::as_str)
                        .is_none_or(known_source)
                });
            }
        }
        let data = Value::Object(data);
        let mut payload = match envelope.kind {
            Kind::Control => Payload::Control(typed(data, "control")?),
            Kind::Ack => Payload::Ack(typed(data, "ack")?),
            Kind::State => {
                let mut state: State = typed(data, "state")?;
                if let Some(items) = &mut state.supported_eebus_use_cases {
                    items.retain(|s| matches!(s.as_str(), "lpc" | "lpp" | "mgcp" | "mpc"));
                }
                Payload::State(state)
            }
            Kind::Read => Payload::Read(typed(data, "read")?),
            Kind::LocalDeviceStatus => {
                Payload::LocalDeviceStatus(typed(data, "localDeviceStatus")?)
            }
        };
        let notify = match &mut payload {
            Payload::Control(c) => c.notify.as_mut(),
            Payload::State(s) => s.notify.as_mut(),
            _ => None,
        };
        if let Some(sources) = notify.and_then(|n| n.source.as_mut()) {
            sources.retain(|s| known_source(s));
        }
        validate_payload(&payload, envelope.relation.is_some())?;
        Ok(Self {
            id: std::mem::take(&mut envelope.id),
            source,
            relation: envelope.relation.take(),
            protocol,
            payload,
        })
    }

    /// Encodes a validated envelope under the default payload and collection budgets.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        enforce_collection_budget(&self.payload, DecodeLimits::default().entries_per_container)?;
        self.validate()?;
        #[derive(Serialize)]
        struct OutboundData<'a> {
            protocol: String,
            #[serde(flatten)]
            body: &'a Payload,
        }
        #[derive(Serialize)]
        struct Outbound<'a> {
            #[serde(rename = "type")]
            kind: Kind,
            id: &'a str,
            source: &'a str,
            specversion: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            relation: Option<&'a str>,
            data: OutboundData<'a>,
        }
        let envelope = Outbound {
            kind: self.payload.kind(),
            id: &self.id,
            source: &self.source,
            specversion: "1.0",
            relation: self.relation.as_deref(),
            data: OutboundData {
                protocol: self.protocol.to_string(),
                body: &self.payload,
            },
        };
        let mut output = BoundedOutput(Vec::new());
        serde_json::to_writer(&mut output, &envelope).map_err(|e| {
            if e.is_io() {
                WireError::TooLarge
            } else {
                WireError::Field("data")
            }
        })?;
        Ok(output.0)
    }
    pub fn validate(&self) -> Result<(), WireError> {
        if self.protocol.major != 1 {
            return Err(WireError::UnsupportedMajor {
                id: self.id.clone(),
                kind: self.payload.kind(),
            });
        }
        require(self.protocol.to_string().len() <= 256, "protocol")?;
        validate_payload(&self.payload, self.relation.is_some())
    }
}

struct BoundedOutput(Vec<u8>);
impl std::io::Write for BoundedOutput {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > MAX_MESSAGE_BYTES - self.0.len() {
            return Err(std::io::Error::other("message limit"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn typed<T: DeserializeOwned>(value: Value, field: &'static str) -> Result<T, WireError> {
    T::deserialize(object::Json(value)).map_err(|_| WireError::Field(field))
}
fn parse_version(value: Option<Value>) -> Result<Version, WireError> {
    value
        .as_ref()
        .and_then(Value::as_str)
        .filter(|s| s.len() <= 256)
        .and_then(|s| Version::parse(s).ok())
        .ok_or(WireError::Field("protocol"))
}
/// SR-001: inspect every object before conversion to serde_json's map, where a second
/// insertion could otherwise erase evidence of duplicate input.
#[derive(Clone, Copy)]
struct CheckedJson<'a> {
    limits: &'a DecodeLimits,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for CheckedJson<'_> {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        if self.depth > self.limits.depth.min(96) {
            return Err(de::Error::custom("nesting limit"));
        }
        // RawValue keeps the JSON token kind and exact numeric lexeme. In particular
        // arbitrary-precision numbers cannot impersonate a map with optional fields.
        let raw = <&RawValue>::deserialize(d)?;
        let token = raw.get();
        let mut parser = serde_json::Deserializer::from_str(token);
        match token.as_bytes()[0] {
            b'{' => parser.deserialize_map(self),
            b'[' => parser.deserialize_seq(self),
            b'"' => String::deserialize(&mut parser).map(Value::String),
            b't' => Ok(Value::Bool(true)),
            b'f' => Ok(Value::Bool(false)),
            b'n' => Ok(Value::Null),
            _ => decimal::number(token).map(Value::Number),
        }
        .map_err(de::Error::custom)
    }
}
impl<'de> Visitor<'de> for CheckedJson<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a bounded JSON container")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let child = Self {
            depth: self.depth + 1,
            ..self
        };
        let mut items = Vec::new();
        while items.len() < self.limits.entries_per_container {
            match seq.next_element_seed(child)? {
                Some(item) => items.push(item),
                None => return Ok(Value::Array(items)),
            }
        }
        if seq.next_element::<de::IgnoredAny>()?.is_some() {
            return Err(de::Error::custom("array limit"));
        }
        Ok(Value::Array(items))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let child = Self {
            depth: self.depth + 1,
            ..self
        };
        let mut items = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if items.len() == self.limits.entries_per_container || items.contains_key(&key) {
                return Err(de::Error::custom("duplicate key or object limit"));
            }
            items.insert(key, map.next_value_seed(child)?);
        }
        Ok(Value::Object(items))
    }
}
