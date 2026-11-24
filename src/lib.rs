//! Backend and local-device components for KEO CLS.EEDI 1.2.0.
//!
//! Decode envelopes with [`wire::Message`], apply protocol transitions in
//! [`engine`] and bind provisioned topic routes with [`transport`]. Enable
//! `mqtt` for broker I/O and `eebus` for equipment limits and meter snapshots.
//! Use [`metering`] to sum signed active power across provisioned devices.
//! Enable `eebus-pairing` to confirm commissioning from an authenticated SHIP session.
//!
//! ```
//! use cls_eedi::wire::{Message, Payload, Read};
//!
//! let query = Message::new("read-7", "operator", Payload::Read(Read {
//!     parameters: vec!["limits".into()],
//! }));
//! let encoded = query.encode()?;
//! assert_eq!(Message::decode(&encoded)?, query);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]

#[cfg(feature = "eebus")]
pub mod eebus;
pub mod engine;
pub mod metering;
pub mod transport;
pub mod wire;
