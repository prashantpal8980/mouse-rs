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

impl Edge {
    /// The edge facing this one. Two screens sit side by side, so leaving through one
    /// edge means arriving through its opposite: any other pairing places the machines
    /// on top of each other and makes the crossing ambiguous.
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }
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
    /// Optional multi-key return/escape combo, e.g. `"ctrl+win+alt+esc"`. Empty = none
    /// (Scroll Lock / `lock_hotkey` still returns control). Matched in the engine.
    #[serde(default)]
    pub return_hotkey: String,

    // server side
    #[serde(default = "default_listen_host")]
    pub listen_host: String,
    #[serde(default = "default_port")]
    pub listen_port: u16,

    // client side
    pub server_host: String,
    #[serde(default = "default_port")]
    pub server_port: u16,
    /// Client only: automatically reconnect (with backoff) after a recoverable
    /// disconnect, and keep retrying when the server isn't up yet. Default on.
    #[serde(default = "default_true")]
    pub reconnect: bool,

    /// Control-panel startup behaviour, for a hands-off start at login (the GUI is
    /// launched by the autostart entry, then brings the link up by itself).
    /// Names match the Python build's config so an existing file carries over.
    #[serde(default)]
    pub autoconnect: bool,
    /// Open the control panel minimized rather than in the foreground.
    #[serde(default)]
    pub start_minimized: bool,

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
            return_hotkey: String::new(),
            listen_host: default_listen_host(),
            listen_port: default_port(),
            // Deliberately empty: a hardcoded example address looks exactly like a
            // configured one, so "restore defaults" used to hand back a plausible
            // wrong answer. Empty is honest, and the panel offers Detect.
            server_host: String::new(),
            server_port: default_port(),
            reconnect: true,
            autoconnect: false,
            start_minimized: false,
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
    ///
    /// The file holds `shared_secret` in the clear, and that secret is what authorises a
    /// peer to inject keystrokes — so on Unix it is written **owner-only (0600)** inside an
    /// owner-only (0700) directory. Both the create mode and an explicit permission set are
    /// applied *before* any bytes are written: `mode()` alone only governs newly created
    /// files, so a config that already exists as 0644 would otherwise keep its old,
    /// world-readable mode and leak the new secret. Windows uses its inherited ACLs.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigError::Io)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Best-effort: a pre-existing dir we don't own shouldn't fail the save.
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let json = serde_json::to_string_pretty(self).map_err(ConfigError::Parse)?;

        // Write a sibling temp file, flush it to disk, then rename over the target. The
        // rename is atomic on both platforms (`MoveFileEx` with REPLACE_EXISTING on
        // Windows), so a reader sees either the old config or the new one and never a
        // half-written file. This matters more than it looks: the GUI saves on every Start,
        // not just when the user presses Save, and the previous truncate-in-place would
        // have destroyed `shared_secret` if the process died mid-write — losing the one
        // value in here that cannot be re-derived.
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));

        let write_tmp = || -> std::io::Result<()> {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // A leftover temp from a previous run may exist with a laxer mode.
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            f.write_all(json.as_bytes())?;
            // Durability before the rename: without this the rename can land while the
            // contents are still only in the page cache, so a power loss leaves an empty
            // config where a valid one used to be.
            f.sync_all()
        };

        if let Err(e) = write_tmp() {
            let _ = std::fs::remove_file(&tmp);
            return Err(ConfigError::Io(e));
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            // The original file is still intact; drop the temp rather than leaving litter.
            let _ = std::fs::remove_file(&tmp);
            return Err(ConfigError::Io(e));
        }
        Ok(())
    }

    // --- Named profiles: saved configs under ~/.mouse_share/profiles/<name>.json ---

    /// Directory holding named config profiles.
    pub fn profiles_dir() -> PathBuf {
        Self::default_path()
            .parent()
            .map(|p| p.join("profiles"))
            .unwrap_or_else(|| PathBuf::from("profiles"))
    }

    /// Path of a named profile (the name is sanitized to a safe filename).
    pub fn profile_path(name: &str) -> Option<PathBuf> {
        let safe = sanitize_profile_name(name)?;
        Some(Self::profiles_dir().join(format!("{safe}.json")))
    }

    /// List saved profile names (sorted, no extension). Empty if the dir is absent.
    pub fn list_profiles() -> Vec<String> {
        list_profiles_in(&Self::profiles_dir())
    }

    /// Load a profile by name.
    pub fn load_profile(name: &str) -> Result<Config, ConfigError> {
        let path = Self::profile_path(name)
            .ok_or_else(|| ConfigError::BadName(name.to_string()))?;
        Self::load(&path)
    }

    /// Save the current config as a named profile.
    pub fn save_profile(&self, name: &str) -> Result<(), ConfigError> {
        let path = Self::profile_path(name)
            .ok_or_else(|| ConfigError::BadName(name.to_string()))?;
        self.save(&path)
    }

    /// Delete a named profile (Ok if it didn't exist).
    pub fn delete_profile(name: &str) -> Result<(), ConfigError> {
        let path = Self::profile_path(name)
            .ok_or_else(|| ConfigError::BadName(name.to_string()))?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }
}

/// Reduce a profile name to a safe single-segment filename: keep alphanumerics,
/// space, `-` and `_`; trim; reject empty / path-like names (no traversal).
pub fn sanitize_profile_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// List `*.json` profile names in `dir` (stem only, sorted). Empty if unreadable.
pub fn list_profiles_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) == Some("json") {
                    path.file_stem().and_then(|s| s.to_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config io error: {0}")]
    Io(#[source] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[source] serde_json::Error),
    #[error("invalid profile name: {0:?}")]
    BadName(String),
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
    fn sanitize_rejects_traversal_and_empty() {
        assert_eq!(sanitize_profile_name("home").as_deref(), Some("home"));
        assert_eq!(sanitize_profile_name("  work laptop ").as_deref(), Some("work laptop"));
        // Path separators / traversal characters are stripped, not honored.
        assert_eq!(sanitize_profile_name("../../etc/passwd").as_deref(), Some("etcpasswd"));
        assert_eq!(sanitize_profile_name("a/b\\c").as_deref(), Some("abc"));
        assert_eq!(sanitize_profile_name(""), None);
        assert_eq!(sanitize_profile_name("/// "), None);
    }

    /// The layout warning keys off this, so a wrong pairing here would silently
    /// approve the nonsensical exit=left/entry=left case it exists to catch.
    #[test]
    fn edges_oppose_and_round_trip() {
        assert_eq!(Edge::Left.opposite(), Edge::Right);
        assert_eq!(Edge::Top.opposite(), Edge::Bottom);
        for e in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            assert_eq!(e.opposite().opposite(), e);
            assert_ne!(e.opposite(), e);
        }
    }

    /// The config carries `shared_secret` in the clear, so it must never be readable by
    /// other local users. Covers both a fresh write and — the case that actually leaks —
    /// overwriting a file that already exists with a permissive mode.
    #[cfg(unix)]
    #[test]
    fn saved_config_is_owner_only_even_when_overwriting_a_lax_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("mouse-rs-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config { shared_secret: "hunter2".into(), ..Default::default() };

        // Fresh file.
        let fresh = dir.join("fresh.json");
        cfg.save(&fresh).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fresh config must be 0600, got {mode:o}");

        // Parent directory must not be listable by others either.
        let dmode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "config dir must be 0700, got {dmode:o}");

        // Pre-existing world-readable file: the old mode must not survive the save.
        let lax = dir.join("lax.json");
        std::fs::write(&lax, "{}").unwrap();
        std::fs::set_permissions(&lax, std::fs::Permissions::from_mode(0o644)).unwrap();
        cfg.save(&lax).unwrap();
        let mode = std::fs::metadata(&lax).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "overwritten config must be tightened to 0600, got {mode:o}");
        assert!(std::fs::read_to_string(&lax).unwrap().contains("hunter2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The GUI saves on every Start, so this path runs constantly. It must replace the file
    /// atomically and leave no temp files behind — a stray `config.tmp1234` beside the real
    /// config would also be a second copy of the secret on disk.
    #[test]
    fn save_replaces_atomically_and_leaves_no_temp_files() {
        let dir = std::env::temp_dir().join(format!("mouse-rs-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");

        let first = Config { name: "one".into(), shared_secret: "s1".into(), ..Default::default() };
        first.save(&path).unwrap();

        let second = Config { name: "two".into(), shared_secret: "s2".into(), ..Default::default() };
        second.save(&path).unwrap();

        // The replacement won, completely — no merged or truncated remnant of the first.
        let back = Config::load(&path).unwrap();
        assert_eq!(back.name, "two");
        assert_eq!(back.shared_secret, "s2");

        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.json")
            .collect();
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profiles_roundtrip_in_a_dir() {
        let dir = std::env::temp_dir().join(format!("mouse-rs-prof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(list_profiles_in(&dir).is_empty(), "starts empty");

        let cfg = Config { name: "p1".into(), ..Default::default() };
        cfg.save(&dir.join("home.json")).unwrap();
        cfg.save(&dir.join("work.json")).unwrap();
        // A non-json file is ignored.
        std::fs::write(dir.join("notes.txt"), "x").unwrap();

        assert_eq!(list_profiles_in(&dir), vec!["home".to_string(), "work".to_string()]);

        let back = Config::load(&dir.join("home.json")).unwrap();
        assert_eq!(back.name, "p1");

        let _ = std::fs::remove_dir_all(&dir);
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
