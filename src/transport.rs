//! Provisioned MQTT routes for one local device and its authorized SHIP peers.
//!
//! KEO leaves base topics to the operator. Match topics before decoding messages:
//! an envelope's `source` field is descriptive and supplies no authorization.
use crate::wire::Kind;
use std::collections::BTreeSet;
use thiserror::Error;

#[cfg(feature = "mqtt")]
pub mod mqtt;

/// Which end of an exchange owns this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Backend,
    LocalDevice,
}

impl Role {
    fn peer(self) -> Self {
        match self {
            Self::Backend => Self::LocalDevice,
            Self::LocalDevice => Self::Backend,
        }
    }
}

/// Address within one local-device route pair.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Local,
    Ship(String),
    Presence,
}

/// A route or publication failed admission. Errors exclude untrusted topic contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RouteError {
    #[error("base topics must be distinct, disjoint MQTT topic trees with nonempty levels")]
    InvalidBase,
    #[error("publication is outside the provisioned routes")]
    Unprovisioned,
    #[error("invalid or noncanonical SHIP identifier encoding")]
    InvalidId,
    #[error("MQTT topic exceeds 65535 bytes")]
    TooLong,
    #[error("retain is permitted and required only on the device status topic")]
    Retain,
    #[error("message type is invalid for this role or topic")]
    Direction,
}

/// Two disjoint base topics and the local allowlist of subsequent devices.
#[derive(Clone, Debug)]
pub struct Routes {
    to_device: String,
    from_device: String,
    ships: BTreeSet<String>,
}

impl Routes {
    /// Provision the incoming and outgoing base topics as seen by the local device.
    pub fn new(
        to_device: impl Into<String>,
        from_device: impl Into<String>,
    ) -> Result<Self, RouteError> {
        let to_device = to_device.into();
        let from_device = from_device.into();
        for base in [&to_device, &from_device] {
            if base.split('/').any(str::is_empty)
                || base.contains(['#', '+'])
                || base.chars().any(char::is_control)
                || base.starts_with("$share/")
            {
                return Err(RouteError::InvalidBase);
            }
            if base.len() + "/deviceStatus".len() > u16::MAX as usize {
                return Err(RouteError::TooLong);
            }
        }
        if to_device == from_device
            || to_device.starts_with(&format!("{from_device}/"))
            || from_device.starts_with(&format!("{to_device}/"))
        {
            return Err(RouteError::InvalidBase);
        }
        Ok(Self {
            to_device,
            from_device,
            ships: BTreeSet::new(),
        })
    }

    /// Authorize the exact, unescaped SHIP ID.
    pub fn with_ship(mut self, id: impl Into<String>) -> Result<Self, RouteError> {
        let id = id.into();
        if id.is_empty() {
            return Err(RouteError::InvalidId);
        }
        let budget = usize::from(u16::MAX)
            - self.to_device.len().max(self.from_device.len())
            - "/ship/".len();
        if id.len() > budget
            || id
                .bytes()
                .map(|b| if reserved(b) { 3 } else { 1 })
                .sum::<usize>()
                > budget
        {
            return Err(RouteError::TooLong);
        }
        self.ships.insert(id);
        Ok(self)
    }

    fn base(&self, sender: Role) -> &str {
        match sender {
            Role::Backend => &self.to_device,
            Role::LocalDevice => &self.from_device,
        }
    }

    /// Return the MQTT publish topic for this sender and target.
    pub fn topic(&self, sender: Role, target: &Target) -> Result<String, RouteError> {
        let base = self.base(sender);
        match target {
            Target::Local => Ok(base.to_owned()),
            Target::Ship(id) if self.ships.contains(id) => {
                Ok(format!("{base}/ship/{}", encoded(id)))
            }
            Target::Ship(_) => Err(RouteError::Unprovisioned),
            Target::Presence if sender == Role::LocalDevice => Ok(format!("{base}/deviceStatus")),
            Target::Presence => Err(RouteError::Direction),
        }
    }

    /// Resolve a publication observed at the receiving end. SR-002.
    pub fn receive(&self, receiver: Role, topic: &str, retain: bool) -> Result<Target, RouteError> {
        if topic.len() > u16::MAX.into() {
            return Err(RouteError::TooLong);
        }
        let base = self.base(receiver.peer());
        let target = if topic == base {
            Target::Local
        } else {
            let suffix = topic
                .strip_prefix(base)
                .and_then(|s| s.strip_prefix('/'))
                .ok_or(RouteError::Unprovisioned)?;
            if receiver == Role::Backend && suffix == "deviceStatus" {
                Target::Presence
            } else {
                let level = suffix
                    .strip_prefix("ship/")
                    .ok_or(RouteError::Unprovisioned)?;
                let id = decoded(level)?;
                if !self.ships.contains(&id) {
                    return Err(RouteError::Unprovisioned);
                }
                Target::Ship(id)
            }
        };
        if retain != (target == Target::Presence) {
            return Err(RouteError::Retain);
        }
        Ok(target)
    }

    /// Enforce message direction after route and retain admission.
    pub fn check_kind(sender: Role, target: &Target, kind: Kind) -> Result<(), RouteError> {
        let allowed = match kind {
            Kind::LocalDeviceStatus => sender == Role::LocalDevice && *target == Target::Presence,
            _ if *target == Target::Presence => false,
            Kind::Control => sender == Role::Backend,
            Kind::State => sender == Role::LocalDevice,
            Kind::Read | Kind::Ack => true,
        };
        if allowed {
            Ok(())
        } else {
            Err(RouteError::Direction)
        }
    }
}

fn reserved(b: u8) -> bool {
    b < 0x20 || matches!(b, 0x7f | b'#' | b'$' | b'+' | b'/' | b' ' | b'%')
}

fn encoded(id: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut result = String::with_capacity(id.len());
    for c in id.chars() {
        if c.is_ascii() && reserved(c as u8) {
            let b = c as u8;
            result.push('%');
            result.push(char::from(HEX[usize::from(b >> 4)]));
            result.push(char::from(HEX[usize::from(b & 15)]));
        } else {
            result.push(c);
        }
    }
    result
}

fn decoded(level: &str) -> Result<String, RouteError> {
    fn hex(b: u8) -> Result<u8, RouteError> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(RouteError::InvalidId),
        }
    }
    if level.is_empty() {
        return Err(RouteError::InvalidId);
    }
    let mut result = Vec::with_capacity(level.len());
    let mut bytes = level.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let high = hex(bytes.next().ok_or(RouteError::InvalidId)?)?;
            let low = hex(bytes.next().ok_or(RouteError::InvalidId)?)?;
            let value = high << 4 | low;
            if !reserved(value) {
                return Err(RouteError::InvalidId);
            }
            result.push(value);
        } else if reserved(b) {
            return Err(RouteError::InvalidId);
        } else {
            result.push(b);
        }
    }
    String::from_utf8(result).map_err(|_| RouteError::InvalidId)
}
