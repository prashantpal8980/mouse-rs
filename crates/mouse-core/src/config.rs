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
fn default_discovery_port() -> u16 {
    crate::beacon::DEFAULT_DISCOVERY_PORT
}
fn default_lock_hotkey() -> String {
    "scroll_lock".into()
}
fn default_edge_threshold() -> i32 {
    2
}
fn default_name() -> String {
    "mouse-rs".into()
}
/// Matches Windows' own clipboard history, so the default feels familiar.
fn default_clip_history() -> usize {
    25
}
fn default_clip_item_bytes() -> usize {
    5 * 1024 * 1024
}
fn default_clip_memory() -> usize {
    64 * 1024 * 1024
}
fn default_clip_image_px() -> usize {
    4096
}
/// Fetch small things immediately; leave anything larger until it is pasted.
///
/// This has now been wrong in both directions. At 1 MB, every real file stopped and
/// waited for a button press in the panel — a step nobody wanted. At unlimited, copying
/// a 7 GB folder immediately pulled 7 GB across whether or not it was ever pasted,
/// which is worse: it spends the network and the disk on a decision the user has not
/// made yet.
///
/// 16 MB is the line between the two. Below it a fetch is over before anyone could
/// notice, so doing it eagerly costs nothing and the paste is instant. Above it, only
/// the description crosses and the transfer starts when a paste actually asks for the
/// content — copying stays free, and pasting is what commits.
fn default_auto_transfer() -> u64 {
    16 * 1024 * 1024
}
/// 64 MB: a document or a photo is hashed in well under a second, a film is not.
fn default_hash_limit() -> u64 {
    64 * 1024 * 1024
}
/// 16 GB. The cache is a staging area for files that are usually on their way somewhere
/// else, and 2 GB was smaller than a single item people actually move — one 4 GB archive
/// put it permanently over budget, which made every later transfer fight the evictor for
/// room it could never win.
fn default_cache_budget() -> u64 {
    16 * 1024 * 1024 * 1024
}
fn default_inline_image() -> usize {
    256 * 1024
}
fn default_max_files() -> usize {
    10_000
}
/// 8 MB: about 32 chunks in flight, which keeps a gigabit link fed while costing less
/// memory than a single screenshot.
fn default_send_buffer_mb() -> u32 {
    8
}
fn default_log_upload_secs() -> u32 {
    30
}
fn default_quick_send() -> u32 {
    3
}

fn default_stream_min_mb() -> u32 {
    64
}

fn default_stream_wait_secs() -> u32 {
    180
}

/// The placeholder secret of a fresh install. A server about to pair replaces it with a
/// random one first, so no two installs end up paired by sharing the placeholder.
pub const DEFAULT_SECRET: &str = "change-me-to-a-long-random-string";

/// On-disk configuration. Field names match the Python version exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub name: String,
    pub role: Role,

    pub shared_secret: String,
    /// The connection password (PIN), kept on the client and typed on the server (R35).
    /// One password for both places below. Empty until the user sets one. Kept in the clear
    /// like `shared_secret`, in the same 0600 file: a short PIN gains nothing from hashing.
    pub connection_pin: String,
    /// Ask for the password while someone is logged in on this client. Off by default
    /// (owner, 27 Oct); the panel reminds once a day until it is on or the reminder is
    /// turned off.
    pub pin_enabled: bool,
    /// Ask for the password at this client's login screen. On by default (owner, 27 Oct):
    /// typing into a login prompt from the network is what most needs guarding.
    #[serde(default = "default_true")]
    pub login_pin_enabled: bool,
    /// "Don't show this again" on the daily set-a-password reminder.
    pub pin_reminder_off: bool,
    /// The day (days since 1970, UTC) the reminder was last dismissed with "Not now".
    pub pin_reminder_day: u64,
    #[serde(default = "default_true")]
    pub tls_enabled: bool,
    #[serde(default = "default_true")]
    pub clipboard_sync: bool,

    // --- clipboard manager ---
    // Every one of these is a trade the owner is entitled to make. The panel shows what
    // a setting costs; it does not refuse a value because the cost is high. Only knobs
    // the code actually honours live here — a setting nothing reads is worse than no
    // setting, because it looks like it works (see R16 in docs/UI-REQUIREMENTS.md).
    /// Sync copied text. Off means text stays on the machine it was copied on.
    #[serde(default = "default_true")]
    pub clipboard_sync_text: bool,
    /// Sync copied images.
    #[serde(default = "default_true")]
    pub clipboard_sync_images: bool,
    /// How many history entries to keep before the oldest unpinned one is dropped.
    #[serde(default = "default_clip_history")]
    pub clipboard_history_size: usize,
    /// Largest single item carried and kept, in bytes — text and encoded images alike.
    #[serde(default = "default_clip_item_bytes")]
    pub clipboard_max_item_bytes: usize,
    /// Total memory the history may hold, in bytes.
    #[serde(default = "default_clip_memory")]
    pub clipboard_memory_budget: usize,
    /// Largest accepted image edge, in pixels. Raising this is briefly expensive: an
    /// image costs `px × px × 4` bytes of RGBA while it is encoded or decoded.
    #[serde(default = "default_clip_image_px")]
    pub clipboard_max_image_px: usize,

    // --- the bulk lane ---
    /// Carry large transfers on a second connection, so they can never queue in front
    /// of a mouse or keyboard frame on the shared link.
    #[serde(default = "default_true")]
    pub bulk_enabled: bool,
    /// Port for that second connection. **0 means "the same port as the input link"**,
    /// which is the default and needs nothing extra opened in a firewall — the lane is
    /// declared in the handshake instead. Set a different port if you would rather keep
    /// the two apart.
    #[serde(default)]
    pub bulk_port: u16,
    /// Copy files between machines, not just text and images.
    #[serde(default = "default_true")]
    pub clipboard_sync_files: bool,
    /// Transfer a file set this size or smaller without asking, in bytes. `0` means
    /// always ask; a very large value means never ask.
    #[serde(default = "default_auto_transfer")]
    pub clipboard_auto_transfer_max: u64,
    /// Disk the file cache may use, in bytes.
    #[serde(default = "default_cache_budget")]
    pub clipboard_cache_budget: u64,
    /// Where transferred files are kept. Empty = `~/.mouse_share/cache`.
    #[serde(default)]
    pub cache_dir: String,
    /// Largest image still sent inline on the input lane, in bytes. Above this the
    /// pixels go by reference over the paced bulk lane instead. Lower it to protect a
    /// slow link harder; raise it to skip the extra round trip on a fast one.
    #[serde(default = "default_inline_image")]
    pub clipboard_inline_image_max: usize,
    /// Most files one copy may include, so a stray click on an enormous folder does not
    /// commit the machine to hashing all of it.
    #[serde(default = "default_max_files")]
    pub clipboard_max_files_per_copy: usize,
    /// Where to ship this machine's log for diagnosis, e.g.
    /// `http://192.168.137.2:8001`. Empty = off, which is the default.
    ///
    /// A two-machine fault is almost never visible from one side, and fetching the
    /// other log by hand is slow enough that in practice the diagnosis becomes a guess
    /// instead. Plain HTTP to a machine the owner names themselves — nothing leaves the
    /// machine unless this is set.
    #[serde(default)]
    pub log_upload_url: String,
    /// How often to ship it, in seconds.
    #[serde(default = "default_log_upload_secs")]
    pub log_upload_secs: u32,
    /// Set once the user has been told that normal privileges limit crossing on
    /// Windows, so the warning is shown **once** rather than every launch.
    ///
    /// Running without administrator rights is a legitimate choice — most people never
    /// meet an elevated window — so this is a one-time notice with a way to act on it,
    /// not a permanent banner nagging about a decision already made.
    #[serde(default)]
    pub elevation_notice_seen: bool,
    /// Whether the owner has already been told what the login-screen service costs.
    ///
    /// Linux. Turning it on means this machine will accept keystrokes over the network
    /// before anyone has authenticated, which is worth saying once and insulting to say
    /// twice — so the explanation gates the *first* enable and never appears again,
    /// whichever way they answer.
    #[serde(default)]
    pub login_screen_notice_seen: bool,
    /// Show a desktop notification when a file is waiting for a decision.
    ///
    /// Never a dialog — see `crates/mouse/src/notify.rs` for why a focus-stealing
    /// prompt would resurrect the keyboard leak.
    #[serde(default = "default_true")]
    pub clipboard_notify: bool,
    /// Largest file whose contents are hashed before a copy is announced, in bytes.
    ///
    /// Hashing reads every byte: a 12.6 GB folder took 65 seconds to hash, and for all
    /// of that time the copy simply had not appeared on the other machine. Above this
    /// limit the file is identified by path, size and modification time instead, which
    /// is instant and still stable, so dedup and resume keep working — and the transfer
    /// is verified by length on arrival rather than by content.
    ///
    /// Raise it if you would rather have content verification than a fast announcement;
    /// set it very high to always hash, as earlier builds did.
    #[serde(default = "default_hash_limit")]
    pub clipboard_hash_limit: u64,
    /// Pasting a received file **moves** it to where it was pasted, instead of copying.
    ///
    /// This is what makes the paste destination the real destination, at no cost. The
    /// file manager never tells us where a paste is going, so we cannot write the bytes
    /// there ourselves — but publishing the selection as a *cut* makes it relocate our
    /// file instead of duplicating it, and a relocation within one filesystem is a
    /// rename. A 40 GB paste therefore moves no bytes and leaves no second copy.
    ///
    /// What is published is a hard link to the cached content, so the move takes the
    /// link and the cache keeps the data — the entry stays usable afterwards.
    #[serde(default = "default_true")]
    pub clipboard_move_on_paste: bool,
    /// Folder that received files are delivered into, e.g. `/home/you/Desktop`.
    ///
    /// Optional and off by default: with `clipboard_move_on_paste` doing the work, the
    /// place you paste *is* the destination. This is for the different want — files
    /// arriving somewhere automatically, with no paste at all.
    ///
    /// Empty = off, and files only reach the clipboard. Set it and a copied file simply
    /// *appears* there when the transfer finishes, with nothing to paste.
    ///
    /// This is what makes a large copy cost its own size once instead of twice. The
    /// delivered file is a hard link to the cached blob — the same data under two names,
    /// not two copies — so a 40 GB file uses 40 GB. Only a destination on a different
    /// filesystem forces a real copy, and that case says so in the log.
    #[serde(default)]
    pub clipboard_deliver_dir: String,
    /// Put a received image on the clipboard as a `.png` file as well as pixels.
    ///
    /// On by default because without it the everyday case fails: no file manager on
    /// either OS can paste pixels, so copy-an-image then paste-into-a-folder did
    /// nothing, and the only way round it was to open the panel and click the entry.
    /// Ctrl+C then Ctrl+V should work wherever you are pasting.
    #[serde(default = "default_true")]
    pub clipboard_images_also_as_file: bool,
    /// Follow symbolic links found inside a copied folder instead of skipping them.
    ///
    /// Off by default because a link pointing outside the folder quietly copies files
    /// the user never selected. On, cycles are detected and refused rather than
    /// followed forever.
    #[serde(default)]
    pub clipboard_follow_symlinks: bool,
    /// Cap transfer speed hard while you are driving the other machine.
    ///
    /// This used to *stop* transfers outright, and that read as a hang: a copy froze the
    /// instant the pointer crossed and resumed when it came back. Slowing achieves the
    /// same protection — the pointer is what matters at that moment — while the transfer
    /// still finishes.
    #[serde(default = "default_true")]
    pub bulk_pause_while_crossed: bool,
    /// Fixed transfer rate in MB/s. `0` adapts to measured input latency, which is the
    /// default and what protects the mouse automatically.
    #[serde(default)]
    pub bulk_rate_limit_mbps: u32,
    /// How much of a file may sit in memory waiting to go out, in MB.
    ///
    /// This is the ceiling that makes a 4 GB transfer cost the same memory as a 4 MB
    /// one: the sender stops reading while this much is queued. Raising it buys a
    /// little throughput on a fast link at the cost of exactly that much RAM; the
    /// default is comfortable on any machine.
    #[serde(default = "default_send_buffer_mb")]
    pub bulk_send_buffer_mb: u32,
    /// Skip the "this is big" prompt when the transfer is estimated to finish within
    /// this many seconds. `0` disables the shortcut and the size limit alone decides.
    ///
    /// Uses *measured* throughput from a previous transfer; with no measurement yet it
    /// does nothing, because guessing would either nag about fast copies or silently
    /// start slow ones.
    #[serde(default = "default_quick_send")]
    pub clipboard_quick_send_secs: u32,
    /// Files at least this many MB, pasted from the other machine, stream straight from
    /// the network into the paste through memory, taking no extra disk. Smaller ones go
    /// through the cache first.
    #[serde(default = "default_stream_min_mb")]
    pub clipboard_stream_min_mb: u32,
    /// How long a paste whose transfer stopped (cable pulled, Wi-Fi dropped) waits for the
    /// connection to come back before it gives up with an error.
    #[serde(default = "default_stream_wait_secs")]
    pub clipboard_stream_wait_secs: u32,
    /// Keep a copy of what has arrived while a paste runs, so a paste cut short even by a
    /// restart continues where it stopped the next time the same file is copied. Costs
    /// the file's size again in disk while it runs; off by default.
    #[serde(default)]
    pub clipboard_keep_resumable_copy: bool,
    /// Keep clipboard history across restarts.
    ///
    /// Off by default, matching Windows. On means clipboard contents — possibly
    /// passwords — are written to disk, which is the owner's call to make and is said
    /// plainly next to the setting.
    #[serde(default)]
    pub clipboard_persist_history: bool,

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
    /// UDP port for the discovery beacons (see `beacon`). The server sends on it and the
    /// client listens on it, so each finds the other on whatever network they share. 0
    /// turns discovery off, and the client then uses only `server_host`.
    #[serde(default = "default_discovery_port")]
    pub discovery_port: u16,
    /// Runtime only, never saved: set by the login-screen instance, and sent in the
    /// handshake so the other machine knows this one is at a password prompt.
    #[serde(skip)]
    pub at_login_screen: bool,
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
    // `warp_threshold_px` (the Python app's "return distance") was dropped on 2026-10-14
    // by the owner's decision: the Rust engine never read it. Old config files that still
    // carry the key load fine, because unknown keys are ignored.
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: default_name(),
            role: Role::Server,
            shared_secret: DEFAULT_SECRET.into(),
            connection_pin: String::new(),
            pin_enabled: false,
            login_pin_enabled: true,
            pin_reminder_off: false,
            pin_reminder_day: 0,
            tls_enabled: true,
            clipboard_sync: true,
            clipboard_sync_text: true,
            clipboard_sync_images: true,
            clipboard_history_size: default_clip_history(),
            clipboard_max_item_bytes: default_clip_item_bytes(),
            clipboard_memory_budget: default_clip_memory(),
            clipboard_max_image_px: default_clip_image_px(),
            bulk_enabled: true,
            // 0 = share the input link's port. Anything else is a deliberate choice.
            bulk_port: 0,
            clipboard_sync_files: true,
            clipboard_auto_transfer_max: default_auto_transfer(),
            clipboard_cache_budget: default_cache_budget(),
            cache_dir: String::new(),
            clipboard_inline_image_max: default_inline_image(),
            clipboard_max_files_per_copy: default_max_files(),
            clipboard_follow_symlinks: false,
            clipboard_images_also_as_file: true,
            clipboard_deliver_dir: String::new(),
            clipboard_move_on_paste: true,
            clipboard_hash_limit: default_hash_limit(),
            clipboard_notify: true,
            elevation_notice_seen: false,
            login_screen_notice_seen: false,
            log_upload_url: String::new(),
            log_upload_secs: default_log_upload_secs(),
            bulk_pause_while_crossed: true,
            bulk_rate_limit_mbps: 0,
            bulk_send_buffer_mb: default_send_buffer_mb(),
            clipboard_quick_send_secs: default_quick_send(),
            clipboard_stream_min_mb: default_stream_min_mb(),
            clipboard_stream_wait_secs: default_stream_wait_secs(),
            clipboard_keep_resumable_copy: false,
            clipboard_persist_history: false,
            lock_hotkey: default_lock_hotkey(),
            return_hotkey: String::new(),
            listen_host: default_listen_host(),
            listen_port: default_port(),
            // Deliberately empty: a hardcoded example address looks exactly like a
            // configured one, so "restore defaults" used to hand back a plausible
            // wrong answer. Empty is honest, and the panel offers Detect.
            server_host: String::new(),
            server_port: default_port(),
            discovery_port: default_discovery_port(),
            at_login_screen: false,
            reconnect: true,
            autoconnect: false,
            start_minimized: false,
            exit_edge: Edge::Left,
            entry_edge: Edge::Right,
            screen_width: 1920,
            screen_height: 1080,
            edge_threshold_px: default_edge_threshold(),
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
        let mut cfg: Config = serde_json::from_str(trimmed).map_err(ConfigError::Parse)?;
        cfg.tidy();
        Ok(cfg)
    }

    /// Clean up values that are harmless in the file but corrupt everything downstream.
    ///
    /// The machine name is the case that bit: trailing spaces in the config travelled
    /// into the handshake (the peer displayed `laptop           `), into the panel, and
    /// into the uploaded log filename as `laptop------------mouse-rs.log`. Nothing in
    /// the program added them and nothing was going to remove them, so a typo made once
    /// stayed visible in three unrelated places for ever.
    ///
    /// Trimmed on load rather than on save, so an already-damaged config is fixed the
    /// next time it is read instead of only after someone happens to press Save.
    fn tidy(&mut self) {
        let name = self.name.trim();
        if name.len() != self.name.len() {
            tracing::info!(
                trimmed = %name,
                "the machine name had surrounding whitespace; using the trimmed form"
            );
            self.name = name.to_string();
        }
        if self.name.is_empty() {
            self.name = default_name();
        }
        // Same reasoning: a stray space in a host or URL fails in a way that reads as a
        // network fault rather than a typo.
        self.listen_host = self.listen_host.trim().to_string();
        self.server_host = self.server_host.trim().to_string();
        self.log_upload_url = self.log_upload_url.trim().to_string();
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
        // The file still carries the dropped `warp_threshold_px`; it must load anyway.
    }

    #[test]
    fn a_name_with_stray_whitespace_is_trimmed_on_load() {
        // This really happened: the Windows config held "laptop" plus twelve spaces, and
        // they surfaced in the handshake, the panel and an uploaded log filename.
        let dir = std::env::temp_dir().join("mouse-rs-cfg-tidy");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("config.json");
        std::fs::write(
            &p,
            br#"{"name":"laptop            ","server_host":" 192.168.137.2 ","log_upload_url":" http://a:1 "}"#,
        )
        .unwrap();

        let c = Config::load(&p).expect("loads");
        assert_eq!(c.name, "laptop");
        assert_eq!(c.server_host, "192.168.137.2");
        assert_eq!(c.log_upload_url, "http://a:1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_that_is_only_whitespace_falls_back_rather_than_becoming_empty() {
        let dir = std::env::temp_dir().join("mouse-rs-cfg-blank");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("config.json");
        std::fs::write(&p, br#"{"name":"   "}"#).unwrap();
        let c = Config::load(&p).expect("loads");
        assert!(!c.name.is_empty(), "an empty name would show as a blank peer everywhere");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
