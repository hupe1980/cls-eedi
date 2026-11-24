use ::eebus::runtime::ShipConnection;

use crate::engine::{EngineError, LocalDevice};

/// Record a completed SHIP pairing using identity from the open connection.
///
/// Capture [`LocalDevice::trust_revision`] after accepting the trust replacement.
/// The host drives the SHIP Pairing Service and authenticates the connection.
/// After the peer supplies its SHIP ID through access methods, pass the same
/// connection here. The fingerprint and SKI both come from its TLS certificate.
/// A session without an advertised ID cannot confirm an entry by this method.
///
/// Returns `true` for the first confirmation and `false` for an identical repeat.
/// Persist a changed device checkpoint before publishing trust readback.
///
/// ```no_run
/// # use cls_eedi::{eebus::confirm_pairing, engine::{EngineError, LocalDevice}};
/// # use eebus::runtime::ShipConnection;
/// fn on_peer_identified(device: &mut LocalDevice, revision: u64, peer: &ShipConnection)
///     -> Result<bool, EngineError>
/// {
///     confirm_pairing(device, revision, peer)
/// }
/// ```
pub fn confirm_pairing(
    device: &mut LocalDevice,
    revision: u64,
    connection: &ShipConnection,
) -> Result<bool, EngineError> {
    if !connection.is_open() {
        return Err(EngineError::PairingMismatch);
    }
    let id = connection
        .peer_ship_id()
        .ok_or(EngineError::PairingMismatch)?;
    device.pairing_established(
        revision,
        id,
        &connection.peer_fingerprint().to_hex(),
        &connection.peer().to_txt_value(),
    )
}
