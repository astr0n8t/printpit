//! Server-side Moonraker client. Every request to a printer goes through here, using the
//! segmented egress client from [`net`].

pub mod client;
pub mod error;
pub mod net;
pub mod types;

pub use client::MoonrakerClient;
pub use error::MoonrakerError;
pub use net::Egress;
