//! Typed envelope discriminants, commands and state.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    #[serde(rename = "de.keo-connectivity.clseedi.control")]
    Control,
    #[serde(rename = "de.keo-connectivity.clseedi.state")]
    State,
    #[serde(rename = "de.keo-connectivity.clseedi.read")]
    Read,
    #[serde(rename = "de.keo-connectivity.clseedi.ack")]
    Ack,
    #[serde(rename = "de.keo-connectivity.clseedi.localDeviceStatus")]
    LocalDeviceStatus,
}

pub(super) fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

/// Active power in watts and optional lifetime from receipt in seconds.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Limit {
    pub value: i64,
    pub active: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub duration: Option<u32>,
}

/// Directions within an active-power property.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase", bound(deserialize = "T: Deserialize<'de>"))]
pub struct Power<T> {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub consumption: Option<T>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub production: Option<T>,
}

/// SHIP pairing material. Debug output is redacted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Pairing {
    pub for_id: String,
    pub for_par: String,
    pub secret: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub ski: Option<String>,
}
impl fmt::Debug for Pairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pairing { [redacted] }")
    }
}

/// One trust credential or pairing operation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TrustEntry {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub ski: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub certificate: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub ship_pairing_service: Option<Pairing>,
}
impl fmt::Debug for TrustEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TrustEntry { [redacted] }")
    }
}

/// An empty object cancels notifications. A configuration supplies all three fields.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Notify {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub interval: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub end_time: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub source: Option<Vec<String>>,
}

/// Exact meter value equal to `number × 10^scale`. The wire property supplies its unit.
///
/// Display uses fixed-point decimal when the signed result fits 32 ASCII characters,
/// preserving fractional trailing zeros. Larger results use the original integer
/// coefficient followed by `e` and the original scale. Zero displays as `0`.
/// Formatting never rounds the measurement. [KEO scaled-number definition](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/schemas/definitions.schema.json)
///
/// ```
/// use cls_eedi::wire::ScaledNumber;
///
/// assert_eq!(ScaledNumber { number: 23050, scale: -2 }.to_string(), "230.50");
/// assert_eq!(ScaledNumber { number: -1, scale: i16::MIN }.to_string(), "-1e-32768");
/// ```
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScaledNumber {
    pub number: i64,
    pub scale: i16,
}

impl fmt::Display for ScaledNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MAX_DECIMAL_LEN: usize = 32;
        if self.number == 0 || self.scale == 0 {
            return fmt::Display::fmt(&self.number, f);
        }

        let mut digits = self.number.unsigned_abs().to_string();
        let places = usize::from(self.scale.unsigned_abs());
        // A fractional value needs a decimal point and at least one integer digit.
        let magnitude_len = if self.scale < 0 {
            digits.len().max(places + 1) + 1
        } else {
            digits.len() + places
        };
        let sign_len = usize::from(self.number < 0 || f.sign_plus());
        if magnitude_len + sign_len > MAX_DECIMAL_LEN {
            use fmt::Write as _;
            write!(digits, "e{}", self.scale)?;
        } else if self.scale >= 0 {
            digits.extend(std::iter::repeat_n('0', places));
        } else if digits.len() > places {
            digits.insert(digits.len() - places, '.');
        } else {
            digits = format!("0.{digits:0>places$}");
        }
        f.pad_integral(self.number >= 0, "", &digits)
    }
}

/// A meter reading in the unit defined by its wire property.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Measurement {
    pub value: ScaledNumber,
}

/// Per-phase readings.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Phases {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub a: Option<Measurement>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub b: Option<Measurement>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub c: Option<Measurement>,
}

/// Phase-to-neutral voltages.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Voltages {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[serde(rename = "a-n")]
    pub a_n: Option<Measurement>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[serde(rename = "b-n")]
    pub b_n: Option<Measurement>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[serde(rename = "c-n")]
    pub c_n: Option<Measurement>,
}

/// Consumed and produced energy readings.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Energy {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub consumed: Option<Measurement>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub produced: Option<Measurement>,
}

/// Total or per-phase power.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct MeterPower {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub total: Option<Measurement>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub phases: Option<Phases>,
}

/// Latest measurements for one source.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Meter {
    pub id: String,
    pub source: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub id_type: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub frequency: Option<Measurement>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub energy: Option<Energy>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub current: Option<Phases>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub voltage: Option<Voltages>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub power: Option<MeterPower>,
}

/// Backend desired settings.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Control {
    #[serde(
        default,
        with = "power_property",
        skip_serializing_if = "Option::is_none"
    )]
    pub limits: Option<Power<Limit>>,
    #[serde(
        default,
        with = "power_property",
        skip_serializing_if = "Option::is_none"
    )]
    pub failsafes: Option<Power<i64>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub trust: Option<Vec<TrustEntry>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub notify: Option<Notify>,
}

/// Local device snapshot, selective reply or unsolicited update.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub timestamp: i64,
    #[serde(
        default,
        with = "power_property",
        skip_serializing_if = "Option::is_none"
    )]
    pub limits: Option<Power<Limit>>,
    #[serde(
        default,
        with = "power_property",
        skip_serializing_if = "Option::is_none"
    )]
    pub failsafes: Option<Power<i64>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub trust: Option<Vec<TrustEntry>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub notify: Option<Notify>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub measurements: Option<Vec<Meter>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[serde(rename = "supportedEebusUseCases")]
    pub supported_eebus_use_cases: Option<Vec<String>>,
}

/// Requested state properties. Empty requests a full snapshot.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Read {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<String>,
}

/// Application outcome. Code zero indicates success.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Ack {
    pub error_number: u64,
}

/// Online publication or offline last will.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub connected: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub timestamp: Option<i64>,
}

mod power_property {
    use super::*;
    #[derive(Serialize, Deserialize)]
    #[serde(bound(deserialize = "T: Deserialize<'de>"))]
    struct Property<T> {
        #[serde(default, deserialize_with = "present")]
        power: Option<Active<T>>,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(bound(deserialize = "T: Deserialize<'de>"))]
    struct Active<T> {
        #[serde(default, deserialize_with = "present")]
        active: Option<T>,
    }

    pub fn serialize<T: Serialize, S: Serializer>(
        directions: &Option<Power<T>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        directions
            .as_ref()
            .map(|active| Property {
                power: Some(Active {
                    active: Some(active),
                }),
            })
            .serialize(serializer)
    }
    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Power<T>>, D::Error> {
        Ok(Some(
            Property::<Power<T>>::deserialize(deserializer)?
                .power
                .and_then(|power| power.active)
                .unwrap_or(Power {
                    consumption: None,
                    production: None,
                }),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Payload {
    Control(Control),
    State(State),
    Read(Read),
    Ack(Ack),
    LocalDeviceStatus(Status),
}
impl Payload {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Control(_) => Kind::Control,
            Self::State(_) => Kind::State,
            Self::Read(_) => Kind::Read,
            Self::Ack(_) => Kind::Ack,
            Self::LocalDeviceStatus(_) => Kind::LocalDeviceStatus,
        }
    }
}
