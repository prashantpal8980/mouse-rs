//! Core of mouse-rs: configuration, wire protocol, authentication, and (Phase 2)
//! the async network link with heartbeat + cancellation.
//!
//! This crate is platform-agnostic. OS input capture/injection lives in separate
//! crates so a stuck input hook can never block the network task and vice-versa.

pub mod auth;
pub mod config;
pub mod net;
pub mod protocol;
pub mod tls;

pub use config::{sanitize_profile_name, Config, Edge, Role};
pub use net::{run_client, run_server, DisconnectReason, LinkChannels, LinkEvent, LinkParams};
pub use protocol::{Button, Message, MessageCodec, PROTOCOL_VERSION};
