//! Runo's management link (Runo 1.0, 4.4 and 4.8), shared by the device
//! daemon (`runo daemon`, "runod") and the management clients so both ends
//! speak the same protocol:
//!
//! - [`identity`]: a persistent Ed25519 key per device and per client; the
//!   device ID is derived from the public key.
//! - [`tls`]: TLS 1.3 with both sides' self-signed certificates, accepted
//!   by the key pinned at pairing rather than by a CA.
//! - [`frame`] and [`proto`]: length-prefixed frames on numbered channels;
//!   channel 0 carries JSON requests, replies and events.
//! - [`feed`]: the Agent as messages, cards and status (4.11).
//! - [`pair`]: the 6-digit confirmation code, from both sides' nonces (one
//!   committed before the other is seen) and the TLS session.
//! - [`lan`]: which addresses may pair.
//! - [`mdns`]: `_runo._tcp` on the LAN, without avahi.
//! - [`client`]: the client side of a connection.

pub mod client;
pub mod feed;
pub mod frame;
pub mod hex;
pub mod identity;
pub mod lan;
pub mod mdns;
pub mod pair;
pub mod proto;
pub mod tls;

pub use identity::{Identity, device_id};
