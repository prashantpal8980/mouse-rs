//! Core of mouse-rs: configuration, wire protocol, authentication, and (Phase 2)
//! the async network link with heartbeat + cancellation.
//!
//! This crate is platform-agnostic. OS input capture/injection lives in separate
//! crates so a stuck input hook can never block the network task and vice-versa.

pub mod auth;
pub mod beacon;
pub mod bulk;
pub mod config;
pub mod discover;
mod motion_udp;
mod playout;
pub mod net;
pub mod pairing;
pub mod pin;
pub mod protocol;
pub mod qos;
pub mod timing;
pub mod tls;
pub mod trust;

pub use config::{sanitize_profile_name, Config, Edge, NetworkPreference, Role};
pub use net::{run_client, run_server, DisconnectReason, LinkChannels, LinkEvent, LinkParams};
pub use protocol::{Button, Message, MessageCodec, PROTOCOL_VERSION};
