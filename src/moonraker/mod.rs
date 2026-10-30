//! Server-side Moonraker client. Every request to a printer goes through here, using the
//! segmented egress client from [`net`].

pub mod client;
pub mod error;
pub mod hub;
pub mod net;
pub mod rpc;
pub mod status;
pub mod types;
pub mod webcam;

pub use client::MoonrakerClient;
pub use error::MoonrakerError;
pub use net::Egress;

use crate::crypto::Cipher;
use crate::db::printers::{API_KEY_CONTEXT, Printer};

pub fn decrypt_api_key(cipher: &Cipher, printer: &Printer) -> Result<Option<String>, MoonrakerError> {
    printer
        .api_key_enc
        .as_deref()
        .map(|enc| {
            cipher
                .open(enc, API_KEY_CONTEXT)
                .map_err(|e| MoonrakerError::Config(format!("API key: {e}")))
        })
        .transpose()
}

/// A client for `printer`, with its API key decrypted and its URL checked against the allowlist.
pub fn client_for(
    egress: &Egress,
    cipher: &Cipher,
    printer: &Printer,
) -> Result<MoonrakerClient, MoonrakerError> {
    MoonrakerClient::new(egress, &printer.base_url, decrypt_api_key(cipher, printer)?)
}
