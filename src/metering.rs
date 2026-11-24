//! Aggregate signed momentary total power from the provisioned controllable devices.
//!
//! KEO's `controllable` source represents all controllable devices in the local
//! network. This function takes each device's validated total active power in watts.
//! The caller identifies the device through its authenticated session, checks
//! measurement quality and selects observations from one sampling window.
//! [KEO measurements](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/doc/clseedi.md#L553-L605)

use crate::wire::ScaledNumber;
use std::collections::{BTreeMap, BTreeSet};

/// Maximum number of devices in one aggregation.
pub const MAX_PARTICIPANTS: usize = 256;
/// Maximum byte length of a local provisioned participant identifier.
pub const MAX_PARTICIPANT_ID_BYTES: usize = 256;

/// A complete, exact controllable power observation could not be constructed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AggregateError {
    #[error("a nonempty set of controllable devices is required")]
    Empty,
    #[error("too many participants or samples")]
    Capacity,
    #[error("participant ID is empty or exceeds the byte limit")]
    InvalidId,
    #[error("participant appears more than once")]
    DuplicateParticipant,
    #[error("sample belongs to an unprovisioned participant")]
    UnexpectedParticipant,
    #[error("more than one sample was supplied for a participant")]
    DuplicateSample,
    #[error("exact total is outside the wire scaled-number representation")]
    Unrepresentable,
}

/// Sum one signed total-power reading in watts per provisioned device.
///
/// Every ID must appear exactly once in `expected`. An absent reading or an
/// explicit `None` (e.g. stale, invalid or out of range) yields `Ok(None)`.
/// Membership errors are reported even if another sample is missing.
/// IDs are opaque, case-sensitive local identifiers. Positive watts indicate
/// consumption, negative watts production.
///
/// The result retains the finest input scale that fits a signed 64-bit coefficient.
/// Zero retains the finest input scale. The calculation never rounds.
///
/// ```
/// use cls_eedi::{metering::controllable_total_power, wire::ScaledNumber};
///
/// let samples = [
///     ("pump", Some(ScaledNumber { number: 1250, scale: 0 })),
///     ("pv", Some(ScaledNumber { number: -3025, scale: -1 })),
/// ];
/// assert_eq!(controllable_total_power(&["pv", "pump"], &samples)?,
///            Some(ScaledNumber { number: 9475, scale: -1 }));
/// # Ok::<(), cls_eedi::metering::AggregateError>(())
/// ```
pub fn controllable_total_power(
    expected: &[&str],
    samples: &[(&str, Option<ScaledNumber>)],
) -> Result<Option<ScaledNumber>, AggregateError> {
    if expected.is_empty() {
        return Err(AggregateError::Empty);
    }
    if expected.len() > MAX_PARTICIPANTS || samples.len() > MAX_PARTICIPANTS {
        return Err(AggregateError::Capacity);
    }
    let mut participants = BTreeSet::new();
    for &id in expected {
        check_id(id)?;
        if !participants.insert(id) {
            return Err(AggregateError::DuplicateParticipant);
        }
    }
    let mut seen = BTreeSet::new();
    for (id, _) in samples {
        check_id(id)?;
        if !participants.contains(id) {
            return Err(AggregateError::UnexpectedParticipant);
        }
        if !seen.insert(id) {
            return Err(AggregateError::DuplicateSample);
        }
    }
    if seen.len() != participants.len() || samples.iter().any(|(_, value)| value.is_none()) {
        return Ok(None);
    }
    let values = samples.iter().filter_map(|(_, v)| v.as_ref());
    exact_sum(values).map(Some)
}

fn check_id(id: &str) -> Result<(), AggregateError> {
    if id.is_empty() || id.len() > MAX_PARTICIPANT_ID_BYTES {
        return Err(AggregateError::InvalidId);
    }
    Ok(())
}

fn exact_sum<'a>(
    values: impl Iterator<Item = &'a ScaledNumber>,
) -> Result<ScaledNumber, AggregateError> {
    let mut columns = BTreeMap::<i32, i128>::new();
    let mut finest = i16::MAX;
    for value in values {
        finest = finest.min(value.scale);
        *columns.entry(i32::from(value.scale)).or_default() += i128::from(value.number);
    }

    // Collapse intersecting decimal columns before checking representability.
    // Signed remainders let carries die out even for negative numbers. Each
    // input contributes at most 19 occupied places plus a bounded sum carry.
    // An empty 65,535-place gap therefore requires no allocation or iteration.
    let mut digits = Vec::new();
    while let Some((place, coefficient)) = columns.pop_first() {
        let digit = coefficient % 10;
        if digit != 0 {
            digits.push((place, digit));
        }
        let carry = coefficient / 10;
        if carry != 0 {
            *columns.entry(place + 1).or_default() += carry;
        }
    }
    let Some(&(lowest, _)) = digits.first() else {
        return Ok(ScaledNumber {
            number: 0,
            scale: finest,
        });
    };
    // A trailing power of ten above i16::MAX must remain in the coefficient.
    let mut scale = lowest.min(i32::from(i16::MAX)) as i16;
    let mut previous = digits.last().expect("nonempty digits").0;
    let mut coefficient = 0i128;
    for (place, digit) in digits.into_iter().rev().chain([(i32::from(scale), 0)]) {
        let factor = 10i128
            .checked_pow((previous - place) as u32)
            .ok_or(AggregateError::Unrepresentable)?;
        coefficient = coefficient
            .checked_mul(factor)
            .and_then(|n| n.checked_add(digit))
            .ok_or(AggregateError::Unrepresentable)?;
        previous = place;
    }
    let mut number = i64::try_from(coefficient).map_err(|_| AggregateError::Unrepresentable)?;
    while scale > finest {
        let Some(shifted) = number.checked_mul(10) else {
            break;
        };
        number = shifted;
        scale -= 1;
    }
    Ok(ScaledNumber { number, scale })
}
