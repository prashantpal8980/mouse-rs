//! Configuration, wire-compatible with the existing Python `~/.mouse_share/config.json`.
//!
//! Unknown fields are ignored and every field has a default, so an old config
//! loads cleanly and a partially-written one still works.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which side of the link this machine plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Has the physical keyboard/mouse and forwards input to the client.
    Server,
    /// Receives input and injects it locally.
    Client,
}

/// A screen edge the cursor can cross through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

fn default_true() -> bool {
    true
}
fn default_listen_host() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    24800
}
fn default_lock_hotkey() -> String {
    "scroll_lock".into()
}
fn default_edge_threshold() -> i32 {
    2
}
fn default_warp_threshold() -> i32 {
    120
}
fn default_name() -> String {
    "mouse-rs".into()
}

/// On-disk configuration. Field names match the Python version exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub name: String,
    pub role: Role,

    pub shared_secret: String,
    #[serde(default = "default_true")]
    pub tls_enabled: bool,
    #[serde(default = "default_true")]
    pub clipboard_sync: bool,
    #[serde(default = "default_lock_hotkey")]
    pub lock_hotkey: String,

    // server side
    #[serde(default = "default_listen_host")]
    pub listen_host: String,
    #[serde(default = "default_port")]
    pub listen_port: u16,

    // client side
    pub server_host: String,
    #[serde(default = "default_port")]
    pub server_port: u16,

    pub exit_edge: Edge,
    pub entry_edge: Edge,

    pub screen_width: i32,
    pub screen_height: i32,

    #[serde(default = "default_edge_threshold")]
    pub edge_threshold_px: i32,
    #[serde(default = "default_warp_threshold")]
    pub warp_threshold_px: i32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: default_name(),
            role: Role::Server,
            shared_secret: "change-me-to-a-long-random-string".into(),
            tls_enabled: true,
            clipboard_sync: true,
            lock_hotkey: default_lock_hotkey(),
            listen_host: default_listen_host(),
            listen_port: default_port(),
            server_host: "192.168.1.50".into(),
            server_port: default_port(),
            exit_edge: Edge::Left,
            entry_edge: Edge::Right,
            screen_width: 1920,
            screen_height: 1080,
            edge_threshold_px: default_edge_threshold(),
            warp_threshold_px: default_warp_threshold(),
        }
    }
}

impl Config {
    /// Default config path: `~/.mouse_share/config.json` (same as the Python app).
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".mouse_share").join("config.json")
    }

    /// Load from a path, tolerating a UTF-8 BOM (the Python GUI used to write one).
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        let cfg: Config = serde_json::from_str(trimmed).map_err(ConfigError::Parse)?;
        Ok(cfg)
    }

    /// Load from the default path, or return defaults if it doesn't exist yet.
    pub fn load_or_default() -> Config {
        let path = Self::default_path();
        match Self::load(&path) {
            Ok(c) => c,
            Err(ConfigError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Config::default()
            }
            Err(e) => {
                tracing::warn!("failed to load config ({e}); using defaults");
                Config::default()
            }
        }
    }

    /// Write to a path (pretty JSON, no BOM), creating parent dirs.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigError::Io)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(ConfigError::Parse)?;
        std::fs::write(path, json).map_err(ConfigError::Io)?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config io error: {0}")]
    Io(#[source] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_python_example_config() {
        // The exact shape the Python app ships in config.example.json.
        let json = r#"{
          "name": "kali-desktop",
          "role": "server",
          "shared_secret": "secret",
          "tls_enabled": true,
          "clipboard_sync": true,
          "lock_hotkey": "scroll_lock",
          "listen_host": "0.0.0.0",
          "listen_port": 24800,
          "server_host": "192.168.1.50",
          "server_port": 24800,
          "exit_edge": "left",
          "entry_edge": "right",
          "screen_width": 2560,
          "screen_height": 1440,
          "edge_threshold_px": 2,
          "warp_threshold_px": 120
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.role, Role::Server);
        assert_eq!(cfg.exit_edge, Edge::Left);
        assert_eq!(cfg.entry_edge, Edge::Right);
        assert_eq!(cfg.screen_width, 2560);
        assert_eq!(cfg.listen_port, 24800);
    }

    #[test]
    fn tolerates_bom_and_roundtrips() {
        let cfg = Config {
            name: "x".into(),
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let with_bom = format!("\u{feff}{json}");
        let trimmed = with_bom.strip_prefix('\u{feff}').unwrap();
        let back: Config = serde_json::from_str(trimmed).unwrap();
        assert_eq!(back.name, "x");
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let json = r#"{
          "name": "min",
          "role": "client",
          "shared_secret": "s",
          "server_host": "10.0.0.1",
          "exit_edge": "left",
          "entry_edge": "right",
          "screen_width": 1920,
          "screen_height": 1080
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.listen_port, 24800);
        assert!(cfg.tls_enabled);
        assert_eq!(cfg.warp_threshold_px, 120);
    }
}
