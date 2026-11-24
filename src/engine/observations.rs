//! Freshness follows the replacement scope of KEO REQ-29 through REQ-37.
use super::{checked, EngineError, Message, Payload, State, FIELDS};
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub(super) struct Observations {
    pub state: Option<State>,
    // Includes clears from full/selective snapshots. At most six entries.
    properties: BTreeMap<&'static str, i64>,
    // A complete inventory also covers absent IDs, without keeping tombstones.
    inventory: Option<i64>,
    meter_times: BTreeMap<String, i64>,
}

fn present(state: &State, field: &str) -> bool {
    match field {
        "limits" => state.limits.is_some(),
        "failsafes" => state.failsafes.is_some(),
        "trust" => state.trust.is_some(),
        "notify" => state.notify.is_some(),
        "supportedEebusUseCases" => state.supported_eebus_use_cases.is_some(),
        "measurements" => state.measurements.is_some(),
        _ => false,
    }
}

fn replace(state: &mut State, input: &State, field: &str) {
    match field {
        "limits" => state.limits.clone_from(&input.limits),
        "failsafes" => state.failsafes.clone_from(&input.failsafes),
        "trust" => state.trust.clone_from(&input.trust),
        "notify" => state.notify.clone_from(&input.notify),
        "supportedEebusUseCases" => state
            .supported_eebus_use_cases
            .clone_from(&input.supported_eebus_use_cases),
        "measurements" => state.measurements.clone_from(&input.measurements),
        _ => unreachable!("field names come from FIELDS"),
    }
}

impl Observations {
    pub fn property_time(&self, field: &str) -> Option<i64> {
        self.properties.get(field).copied()
    }

    pub fn meter_time(&self, id: &str) -> Option<i64> {
        self.meter_times.get(id).copied().or_else(|| {
            let meters = self.state.as_ref()?.measurements.as_ref()?;
            if meters.iter().any(|meter| meter.id == id) {
                self.inventory
            } else {
                None
            }
        })
    }

    fn merge_meters(
        &mut self,
        state: &mut State,
        items: &[crate::wire::Meter],
        at: i64,
        capacity: usize,
    ) -> Result<bool, EngineError> {
        if self.inventory.is_some_and(|t| at < t) {
            return Ok(false);
        }
        let mut applied = false;
        for item in items {
            if self.meter_times.get(&item.id).is_some_and(|t| at < *t) {
                continue;
            }
            let values = state.measurements.get_or_insert_with(Vec::new);
            if let Some(slot) = values.iter_mut().find(|m| m.id == item.id) {
                slot.clone_from(item);
            } else if values.len() < capacity {
                values.push(item.clone());
            } else {
                return Err(EngineError::Capacity);
            }
            self.meter_times.insert(item.id.clone(), at);
            applied = true;
        }
        Ok(applied)
    }

    /// Stage both values and clocks. A failed capacity or wire check publishes neither.
    pub fn apply(
        &mut self,
        input: &State,
        read: Option<(&[String], Option<i64>)>,
        capacity: usize,
        source: &str,
    ) -> Result<Option<Vec<&'static str>>, EngineError> {
        let at = input.timestamp;
        let mut next = self.clone();
        let mut state = next.state.take().unwrap_or_default();
        let mut changed = Vec::new();
        if let Some((fields, raced_at)) = read {
            let selected = |key: &str| fields.is_empty() || fields.iter().any(|s| s == key);
            if FIELDS
                .iter()
                .any(|key| !selected(key) && present(input, key))
            {
                return Err(EngineError::Unexpected);
            }
            if raced_at.is_some_and(|since| at <= since)
                || self
                    .properties
                    .iter()
                    .any(|(key, t)| selected(key) && at < *t)
            {
                return Ok(None);
            }
            for field in FIELDS.into_iter().filter(|key| selected(key)) {
                replace(&mut state, input, field);
                if field == "measurements" {
                    next.inventory = Some(at);
                    next.meter_times.clear();
                }
                changed.push(field);
            }
        } else {
            for field in FIELDS.into_iter().filter(|key| present(input, key)) {
                if field == "measurements" {
                    let items = input.measurements.as_deref().unwrap_or_default();
                    if !next.merge_meters(&mut state, items, at, capacity)? {
                        continue;
                    }
                } else {
                    if self.properties.get(field).is_some_and(|t| at < *t) {
                        continue;
                    }
                    replace(&mut state, input, field);
                }
                changed.push(field);
            }
            if changed.is_empty()
                && FIELDS.iter().any(|field| match *field {
                    "measurements" => input.measurements.as_ref().is_some_and(|m| !m.is_empty()),
                    key => present(input, key),
                })
            {
                return Ok(None);
            }
        }
        if state
            .measurements
            .as_ref()
            .is_some_and(|m| m.len() > capacity)
        {
            return Err(EngineError::Capacity);
        }
        for field in &changed {
            next.properties
                .entry(field)
                .and_modify(|t| *t = (*t).max(at))
                .or_insert(at);
        }
        state.timestamp = state.timestamp.max(at);
        checked(Message::new(
            "validate",
            source,
            Payload::State(state.clone()),
        ))?;
        next.state = Some(state);
        *self = next;
        Ok(Some(changed))
    }
}
