//! Semantic and collection checks shared by both wire directions.

use super::{Limit, Notify, Payload, Power, TrustEntry, WireError};
use std::collections::BTreeSet;

pub(super) fn enforce_collection_budget(payload: &Payload, max: usize) -> Result<(), WireError> {
    let (trust, notify) = match payload {
        Payload::Control(c) => (c.trust.as_ref(), c.notify.as_ref()),
        Payload::State(s) => {
            require(
                s.measurements.as_ref().is_none_or(|m| m.len() <= max)
                    && s.supported_eebus_use_cases
                        .as_ref()
                        .is_none_or(|v| v.len() <= max),
                "state collection limit",
            )?;
            (s.trust.as_ref(), s.notify.as_ref())
        }
        Payload::Read(r) => return require(r.parameters.len() <= max, "read parameter limit"),
        _ => return Ok(()),
    };
    require(
        trust.is_none_or(|t| t.len() <= max)
            && notify
                .and_then(|n| n.source.as_ref())
                .is_none_or(|v| v.len() <= max),
        "control collection limit",
    )
}

pub(super) fn validate_payload(payload: &Payload, related: bool) -> Result<(), WireError> {
    match payload {
        Payload::Control(c) => {
            let count = [
                c.limits.is_some(),
                c.failsafes.is_some(),
                c.trust.is_some(),
                c.notify.is_some(),
            ]
            .into_iter()
            .filter(|x| *x)
            .count();
            protocol(related || count == 1, "standalone control property count")?;
            validate_power(c.limits.as_ref(), c.failsafes.as_ref(), true)?;
            validate_trust(c.trust.as_deref(), true)?;
            validate_notify(c.notify.as_ref())
        }
        Payload::State(s) => {
            require(s.timestamp >= 0, "timestamp")?;
            validate_power(s.limits.as_ref(), s.failsafes.as_ref(), false)?;
            validate_trust(s.trust.as_deref(), false)?;
            validate_notify(s.notify.as_ref())?;
            if let Some(meters) = &s.measurements {
                let mut ids = BTreeSet::new();
                for meter in meters {
                    require(ids.insert(&meter.id), "duplicate measurement id")?;
                    require(
                        meter.id.chars().count() <= 256
                            && meter
                                .id_type
                                .as_ref()
                                .is_none_or(|s| s.chars().count() <= 64),
                        "measurement id",
                    )?;
                    require(known_source(&meter.source), "measurement source")?;
                    if let Some(v) = &meter.voltage {
                        require(
                            v.a_n.is_some() || v.b_n.is_some() || v.c_n.is_some(),
                            "voltage phase",
                        )?;
                    }
                    for v in [
                        meter.current.as_ref(),
                        meter.power.as_ref().and_then(|p| p.phases.as_ref()),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        require(
                            v.a.is_some() || v.b.is_some() || v.c.is_some(),
                            "measurement phase",
                        )?;
                    }
                }
            }
            Ok(())
        }
        Payload::LocalDeviceStatus(s) => require(
            s.connected == s.timestamp.is_some() && s.timestamp.is_none_or(|t| t >= 0),
            "status timestamp",
        ),
        // The response to a read is a state or control. A read itself starts an exchange.
        Payload::Read(_) => protocol(!related, "read relation"),
        Payload::Ack(_) => Ok(()),
    }
}

pub(super) fn require(test: bool, field: &'static str) -> Result<(), WireError> {
    if test {
        Ok(())
    } else {
        Err(WireError::Field(field))
    }
}
fn protocol(test: bool, rule: &'static str) -> Result<(), WireError> {
    if test {
        Ok(())
    } else {
        Err(WireError::Protocol(rule))
    }
}

pub(super) fn known_source(s: &str) -> bool {
    matches!(s, "gcp" | "controllable")
}
fn validate_power(
    limits: Option<&Power<Limit>>,
    failsafes: Option<&Power<i64>>,
    control: bool,
) -> Result<(), WireError> {
    if let Some(d) = limits {
        let count = usize::from(d.consumption.is_some()) + usize::from(d.production.is_some());
        protocol(!control || count == 1, "limit direction")?;
        for l in [d.consumption.as_ref(), d.production.as_ref()]
            .into_iter()
            .flatten()
        {
            require(l.value >= 0, "limit watts")?;
            if control {
                // Some peers use zero watts with active=false to clear a limit.
                protocol(!l.active || l.value > 0, "active limit watts")?;
                protocol(l.duration != Some(0), "control duration")?;
            }
        }
    }
    if let Some(d) = failsafes {
        protocol(
            !control || (d.consumption.is_some() ^ d.production.is_some()),
            "failsafe direction",
        )?;
        require(
            [d.consumption, d.production]
                .into_iter()
                .flatten()
                .all(|w| w >= 0),
            "failsafe watts",
        )?;
    }
    Ok(())
}
fn validate_notify(notify: Option<&Notify>) -> Result<(), WireError> {
    if let Some(n) = notify {
        if n != &Notify::default() {
            require(n.end_time.is_none_or(|t| t >= 0), "notify endTime")?;
            protocol(
                n.interval.is_some_and(|i| i > 0) && n.end_time.is_some() && n.source.is_some(),
                "notify configuration",
            )?;
        }
    }
    Ok(())
}
fn valid_hex(s: &str, len: Option<usize>) -> bool {
    !s.is_empty()
        && s.len().is_multiple_of(2)
        && len.is_none_or(|n| n == s.len())
        && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn validate_trust(entries: Option<&[TrustEntry]>, control: bool) -> Result<(), WireError> {
    for entry in entries.into_iter().flatten() {
        let count = usize::from(entry.ski.is_some())
            + usize::from(entry.certificate.is_some())
            + usize::from(entry.ship_pairing_service.is_some());
        protocol(count == 1, "trust credential count")?;
        if let Some(v) = &entry.ski {
            require(valid_hex(v, Some(40)), "trust ski")?;
        }
        if let Some(v) = &entry.certificate {
            require(control && valid_hex(v, None), "trust certificate")?;
        }
        if let Some(v) = &entry.ship_pairing_service {
            require(
                !v.for_id.is_empty()
                    && valid_hex(&v.for_par, Some(64))
                    && valid_hex(&v.secret, Some(32)),
                "pairing credential",
            )?;
            if let Some(ski) = &v.ski {
                require(!control && valid_hex(ski, Some(40)), "pairing ski")?;
            }
        }
    }
    Ok(())
}
