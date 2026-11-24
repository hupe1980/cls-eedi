//! Bind SHIP state readback to a complete trust replacement and its equipment outcome.
use super::*;
use crate::wire::Pairing;

/// Equipment evidence for one distinct DER certificate in a trust replacement.
///
/// After verifying and installing the certificate, supply its original hex-encoded
/// DER and the SKI observed by the SHIP implementation. The adapter performs the
/// X.509 and key checks. The engine binds this result to the submitted DER bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct InstalledCertificate {
    pub certificate: String,
    pub ski: String,
}

impl std::fmt::Debug for InstalledCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstalledCertificate { [redacted] }")
    }
}

pub(super) fn confirmed_trust(
    requested: &[TrustEntry],
    certificates: Vec<InstalledCertificate>,
) -> Result<Vec<TrustEntry>, EngineError> {
    let mut by_der: BTreeMap<_, Option<String>> = requested
        .iter()
        .filter_map(|entry| {
            entry
                .certificate
                .as_deref()
                .map(|der| (der.to_ascii_lowercase(), None))
        })
        .collect();
    if certificates.len() != by_der.len() {
        return Err(EngineError::TrustReadback);
    }
    for cert in certificates {
        if cert.certificate.len() > crate::wire::MAX_MESSAGE_BYTES
            || cert.ski.len() != 40
            || !cert.ski.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(EngineError::TrustReadback);
        }
        let slot = by_der
            .get_mut(&cert.certificate.to_ascii_lowercase())
            .ok_or(EngineError::TrustReadback)?;
        if slot.replace(cert.ski).is_some() {
            return Err(EngineError::TrustReadback);
        }
    }
    requested
        .iter()
        .map(|entry| {
            if let Some(der) = &entry.certificate {
                let ski = by_der
                    .get(&der.to_ascii_lowercase())
                    .and_then(Option::as_ref)
                    .ok_or(EngineError::TrustReadback)?;
                Ok(TrustEntry {
                    ski: Some(ski.clone()),
                    ..Default::default()
                })
            } else {
                Ok(entry.clone())
            }
        })
        .collect()
}

impl LocalDevice {
    /// Import a previously commissioned equipment inventory once on a fresh local
    /// route. Later changes use a confirmed replacement or [Self::pairing_established].
    pub fn initialize_trust(&mut self, trust: Vec<TrustEntry>) -> Result<(), EngineError> {
        if self.journal.provisioning.subsequent || self.journal.trust_revision != 0 {
            return Err(EngineError::Unexpected);
        }
        if self.pending().any(|(_, c)| c.trust.is_some()) {
            return Err(EngineError::ReconciliationRequired);
        }
        self.update_observations(|state| state.trust = Some(trust))?;
        self.journal.trust_revision = 1;
        Ok(())
    }

    /// Revision of the active local trust replacement. Capture this value when starting
    /// asynchronous SHIP pairing. Each newly accepted replacement advances the revision,
    /// including a successful clear. A replayed control returns its saved outcome and
    /// leaves the revision unchanged. Device checkpoints preserve it on restart.
    pub fn trust_revision(&self) -> Option<u64> {
        (self.journal.trust_revision != 0).then_some(self.journal.trust_revision)
    }

    /// Record the authenticated SHIP peer's SKI in its original pairing entry.
    /// Returns true for a new observation and false for an identical duplicate.
    /// The revision, case-sensitive SHIP ID and certificate fingerprint must match.
    /// A pending replacement requires reconciliation before reporting a result.
    /// On success, checkpoint the updated inventory before publishing its readback. The
    /// original pairing ID, fingerprint and secret remain in the authorized state response.
    /// [KEO 1.2.0 state trust semantics](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/doc/clseedi.md#trust-statetrust)
    pub fn pairing_established(
        &mut self,
        revision: u64,
        for_id: &str,
        for_par: &str,
        ski: &str,
    ) -> Result<bool, EngineError> {
        if self.trust_revision() != Some(revision) || self.journal.provisioning.subsequent {
            return Err(EngineError::PairingMismatch);
        }
        if self.pending().any(|(_, c)| c.trust.is_some()) {
            return Err(EngineError::ReconciliationRequired);
        }
        let mut peers = self
            .journal
            .state
            .trust
            .iter()
            .flatten()
            .enumerate()
            .filter_map(|(i, entry)| {
                let pairing = entry.ship_pairing_service.as_ref()?;
                (pairing.for_id == for_id && pairing.for_par.eq_ignore_ascii_case(for_par))
                    .then_some((i, pairing))
            });
        let (index, peer) = peers.next().ok_or(EngineError::PairingMismatch)?;
        if peers.next().is_some() || ski.len() != 40 || !ski.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(EngineError::PairingMismatch);
        }
        if let Some(known) = &peer.ski {
            return if known.eq_ignore_ascii_case(ski) {
                Ok(false)
            } else {
                Err(EngineError::PairingMismatch)
            };
        }
        let updated = Pairing {
            ski: Some(ski.to_owned()),
            ..peer.clone()
        };
        self.update_observations(move |state| {
            // This index was selected in the same inventory before the candidate clone.
            state.trust.as_mut().expect("existing inventory")[index].ship_pairing_service =
                Some(updated);
        })?;
        Ok(true)
    }
}
