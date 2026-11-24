//! Counts-only inspection of a validated local-device trust state.
use super::{State, TrustEntry};
use serde::Serialize;

/// A snapshot of commissioning status suitable for operator diagnostics.
///
/// Counts refer to entries in this state message. A missing trust property
/// remains absent information (JSON `null`), while an empty list yields zeros.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct TrustReport {
    pub timestamp: i64,
    pub trust: Option<TrustSummary>,
}

/// Direct entries include SKIs resulting from certificate-based commissioning.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustSummary {
    pub direct_ski: usize,
    pub pairing_pending: usize,
    pub pairing_established: usize,
}

impl TrustSummary {
    /// Count entries from a validated state trust list.
    pub fn from_entries(entries: &[TrustEntry]) -> Self {
        let mut counts = Self::default();
        for entry in entries {
            if let Some(pairing) = &entry.ship_pairing_service {
                if pairing.ski.is_some() {
                    counts.pairing_established += 1;
                } else {
                    counts.pairing_pending += 1;
                }
            } else if entry.ski.is_some() {
                counts.direct_ski += 1;
            }
        }
        counts
    }
}

impl TrustReport {
    /// Project a state obtained from [`super::Message::decode`].
    pub fn from_state(state: &State) -> Self {
        Self {
            timestamp: state.timestamp,
            trust: state.trust.as_deref().map(TrustSummary::from_entries),
        }
    }
}
