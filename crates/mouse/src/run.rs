//! Engine lifecycle: build the network link + edge-crossing engine for a role and
//! run it until cancelled. Shared by the headless CLI (`main`) and the GUI's
//! Start/Stop button so both drive the **exact same** startup and teardown path —
//! the lifecycle that guarantees Stop never hangs and input is never left stuck.

use std::time::Duration;

use mouse_clipboard::{ClipCommandRx, ClipboardHistory, ClipboardSettings, ClipboardSync, HistoryLimits};
use mouse_core::bulk::{BulkChannels, BulkServer, BulkSettings, LinkHealth};
use mouse_core::net::{run_client, run_server, LinkParams};
use mouse_core::protocol::Message;
use mouse_core::{Config, Role};
use mouse_engine::{
    run_client_engine, run_server_engine, ClipboardRoute, EngineLink, Injector, StatusHandle,
};
use mouse_input::{new_injector, screen_size, start_capture, NamedKey};
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

/// Whether this process has a desktop session of its own to inject into.
///
/// The distinction only bites on Linux. X11 injection talks to one specific X server
/// through `DISPLAY` and an auth cookie, which is unavailable before anyone has logged
/// in — so an instance meant to serve the login screen has to reach the input stack
/// underneath the session instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Session {
    /// The normal case: running inside the user's session.
    #[default]
    Desktop,
    /// Running before login, with no session of our own. Injects through the kernel's
    /// uinput device, which is session-independent but needs privilege.
    Greeter,
}

/// Run the engine for `cfg.role` on the **current thread** until `cancel` fires.
/// Blocks until the link/engine tasks finish and the input backends are torn down.
///
/// `install_ctrl_c` wires Ctrl-C → `cancel` (the headless CLI uses it; the GUI owns
/// its own Stop button and passes `false`).
pub fn run_blocking(
    cfg: Config,
    cancel: CancellationToken,
    install_ctrl_c: bool,
    status: Option<StatusHandle>,
    clip_history: Option<ClipboardHistory>,
    clip_commands: Option<ClipCommandRx>,
    health: Option<std::sync::Arc<LinkHealth>>,
) -> anyhow::Result<()> {
    run_blocking_in(
        cfg,
        cancel,
        install_ctrl_c,
        status,
        clip_history,
        clip_commands,
        health,
        Session::Desktop,
    )
}

/// As [`run_blocking`], naming the kind of session this process is running in.
#[allow(clippy::too_many_arguments)]
pub fn run_blocking_in(
    mut cfg: Config,
    cancel: CancellationToken,
    install_ctrl_c: bool,
    status: Option<StatusHandle>,
    clip_history: Option<ClipboardHistory>,
    clip_commands: Option<ClipCommandRx>,
    health: Option<std::sync::Arc<LinkHealth>>,
    session: Session,
) -> anyhow::Result<()> {
    // Use the real screen size for edge geometry and the handshake, so crossing
    // math matches the surface the OS actually clamps the cursor against.
    let local_dims = resolve_dims(&mut cfg);

    tracing::info!(
        name = %cfg.name,
        role = ?cfg.role,
        tls = cfg.tls_enabled,
        "starting mouse-rs for {}x{} screen",
        local_dims.0,
        local_dims.1,
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let params = LinkParams::default();
    // Shared view of input-link latency. The input pump writes its heartbeat
    // round-trip here and the bulk lane paces against it, so a big transfer backs off
    // from the very thing it would otherwise damage.
    //
    // Taken from the caller when there is one: the panel reads throughput off the same
    // object, and two instances would mean it watches one nothing ever writes to.
    let health = health.unwrap_or_default();
    let mut bulk_wiring = setup_bulk(&cfg, health.clone());
    let (link_channels, mut engine_link) = EngineLink::new();
    // Wire the UI's status sink in (the headless CLI passes `None`).
    engine_link.status = status;
    // And the shared health, so crossings pause the bulk lane in *both* builds.
    engine_link.health = Some(health.clone());

    match cfg.role {
        Role::Server => {
            if session == Session::Greeter {
                anyhow::bail!(
                    "the login-screen instance can only be a client: serving would mean \
                     capturing this machine's own keyboard before anyone has logged in, \
                     which is a different capability from injecting and is not offered"
                );
            }
            let (cap_tx, cap_rx) = unbounded_channel();
            // Pass the configured lock/return hotkey so the Windows capture failsafe
            // can self-release suppression on it (the Linux backend ignores it).
            let (capture, suppressor) =
                start_capture(cap_tx, NamedKey::from_name(&cfg.lock_hotkey))?;
            let (clip_route, clipboard) = setup_clipboard(
                &cfg,
                &engine_link.to_peer,
                clip_history.clone(),
                clip_commands,
                bulk_wiring.link.take(),
            );

            let cfg_link = cfg.clone();
            let cfg_engine = cfg.clone();
            let c1 = cancel.clone();
            let c2 = cancel.clone();
            // With a separate bulk port the shared listener refuses bulk connections
            // and a second listener serves them instead; with the default (0) the two
            // lanes share one port and one listener.
            let separate_bulk_port = bulk_port(&cfg) != cfg.listen_port;
            let bulk_server = match bulk_wiring.channels.take() {
                Some(ch) => BulkServer::enabled(ch, health.clone(), bulk_settings(&cfg)),
                None => BulkServer::disabled(),
            };
            let (shared_bulk, extra_bulk) = if separate_bulk_port {
                (BulkServer::disabled(), Some(bulk_server))
            } else {
                (bulk_server, None)
            };
            let cfg_bulk_listener = cfg.clone();
            let bulk_listen_port = bulk_port(&cfg);
            let health_link = health.clone();
            runtime.block_on(async move {
                if install_ctrl_c {
                    spawn_ctrl_c(cancel.clone());
                }
                let mut link = tokio::spawn(run_server(
                    cfg_link,
                    params,
                    c1,
                    link_channels,
                    shared_bulk,
                    health_link,
                ));
                if let Some(b) = extra_bulk {
                    tokio::spawn(mouse_core::net::run_bulk_listener(
                        cfg_bulk_listener,
                        bulk_listen_port,
                        params,
                        cancel.clone(),
                        b,
                    ));
                }
                let mut engine = tokio::spawn(run_server_engine(
                    cfg_engine,
                    local_dims,
                    engine_link,
                    suppressor,
                    cap_rx,
                    c2,
                    clip_route,
                ));
                tokio::select! {
                    _ = &mut link => { cancel.cancel(); let _ = engine.await; }
                    _ = &mut engine => { cancel.cancel(); let _ = link.await; }
                }
            });

            // Off-reactor teardown: release the X grab and join the capture thread.
            capture.stop();
            if let Some(c) = clipboard {
                c.stop();
            }
        }
        Role::Client => {
            let injector = Injector::spawn(injector_for(session, local_dims)?)?;
            let inject_tx = injector.sender();
            let (clip_route, clipboard) = setup_clipboard(
                &cfg,
                &engine_link.to_peer,
                clip_history.clone(),
                clip_commands,
                bulk_wiring.link.take(),
            );

            let cfg_link = cfg.clone();
            let cfg_engine = cfg.clone();
            let cfg_bulk_client = cfg.clone();
            let c1 = cancel.clone();
            let c2 = cancel.clone();
            let bulk_channels = bulk_wiring.channels.take();
            let health_link = health.clone();
            runtime.block_on(async move {
                if install_ctrl_c {
                    spawn_ctrl_c(cancel.clone());
                }
                let mut link = tokio::spawn(run_client(
                    cfg_link,
                    params,
                    c1,
                    link_channels,
                    health_link.clone(),
                ));
                // The bulk lane is best-effort: it retries quietly and its failure never
                // touches the input link. A machine without it is slower, not broken.
                if let Some(mut bulk_ch) = bulk_channels {
                    let cfg_bulk = cfg_bulk_client;
                    let c3 = cancel.clone();
                    let h = health_link.clone();
                    tokio::spawn(async move {
                        run_bulk_client_forever(cfg_bulk, params, c3, &mut bulk_ch, h).await;
                    });
                }
                let mut engine = tokio::spawn(run_client_engine(
                    cfg_engine,
                    local_dims,
                    engine_link,
                    inject_tx,
                    c2,
                    clip_route,
                ));
                tokio::select! {
                    _ = &mut link => { cancel.cancel(); let _ = engine.await; }
                    _ = &mut engine => { cancel.cancel(); let _ = link.await; }
                }
            });

            // Off-reactor teardown: release any held keys/buttons, join the thread.
            injector.shutdown();
            if let Some(c) = clipboard {
                c.stop();
            }
        }
    }

    // Bounded drop so a stray blocking task can never hang process exit.
    runtime.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

/// A running engine owned by the GUI: Start spawns a dedicated thread with its own
/// tokio runtime; Stop cancels it. The cancel is non-blocking so the UI thread never
/// freezes; the bounded teardown happens on the engine thread (and on `Drop`).
#[cfg(feature = "gui")]
pub struct EngineHandle {
    cancel: CancellationToken,
    thread: Option<std::thread::JoinHandle<()>>,
    status: StatusHandle,
    clip_history: ClipboardHistory,
    clip_commands: mouse_clipboard::ClipboardCommander,
    /// Whether the owner asked for the history to survive a restart.
    persist: bool,
    cache_dir: String,
    health: std::sync::Arc<LinkHealth>,
}

#[cfg(feature = "gui")]
impl EngineHandle {
    /// Start the engine for `cfg.role` on a background thread. Returns immediately.
    pub fn start(cfg: Config) -> EngineHandle {
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let status = StatusHandle::new();
        let s = status.clone();
        let health = LinkHealth::new();
        let health_for_engine = health.clone();
        let clip_history = ClipboardHistory::with_limits(clipboard_limits(&cfg));
        // Always loaded. What was *written* is what the privacy setting governs, so an
        // unwanted text entry is never on disk to be read back — but a file transfer
        // interrupted by a restart is, and can be resumed instead of lost.
        {
            let cache = mouse_clipboard::cache::BlobCache::new(if cfg.cache_dir.trim().is_empty() {
                mouse_clipboard::cache::BlobCache::default_root()
            } else {
                std::path::PathBuf::from(&cfg.cache_dir)
            })
            .ok();
            clip_history.load(&history_path(), cache.as_ref());
        }
        let persist = cfg.clipboard_persist_history;
        let cache_dir = cfg.cache_dir.clone();
        let ch = clip_history.clone();
        // The panel keeps the sending half so it can ask for an entry to be put back on
        // the clipboard; the receiving half goes to the clipboard thread, which is the
        // only thread allowed to touch the OS clipboard.
        let (clip_commands, clip_command_rx) = mouse_clipboard::command_channel();
        let thread = std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || {
                if let Err(e) = run_blocking(
                    cfg,
                    c,
                    false,
                    Some(s),
                    Some(ch),
                    Some(clip_command_rx),
                    Some(health_for_engine),
                ) {
                    tracing::error!("engine exited with error: {e}");
                }
            })
            .expect("spawn engine thread");
        EngineHandle {
            cancel,
            thread: Some(thread),
            status,
            clip_history,
            clip_commands,
            persist,
            cache_dir,
            health,
        }
    }

    /// Ask the engine to stop. Non-blocking: the engine tears down on its own thread.
    pub fn request_stop(&self) {
        self.cancel.cancel();
    }

    /// True while the engine thread is still alive.
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// A live snapshot of the link status (connected/engaged/peer) for the UI.
    pub fn status(&self) -> mouse_engine::StatusSnapshot {
        self.status.snapshot()
    }

    /// The clipboard history as display rows (newest first). Carries no payloads, so
    /// the panel can call this every redraw without copying clipboard contents.
    pub fn clipboard_meta(&self) -> Vec<mouse_clipboard::ClipMeta> {
        self.clip_history.meta()
    }

    /// Entry count and total bytes held, so the panel can show what the user's own
    /// limits are actually costing.
    pub fn clipboard_usage(&self) -> (usize, usize) {
        self.clip_history.usage()
    }

    /// Put a history entry back on the clipboard (and send it to the peer). Goes to the
    /// clipboard thread rather than being done here: this is the UI thread, and the OS
    /// clipboard has exactly one owner in this process.
    pub fn clipboard_reuse(&self, id: u64) {
        self.clip_commands.reuse(id);
    }

    /// Disk the transferred-file cache is using, and how many files that is.
    pub fn cache_usage(&self) -> Option<(u64, usize)> {
        mouse_clipboard::cache::BlobCache::new(self.cache_root()).ok().map(|c| c.usage())
    }

    /// Where the cache actually lives: the configured path, or the default when blank.
    fn cache_root(&self) -> std::path::PathBuf {
        if self.cache_dir.trim().is_empty() {
            mouse_clipboard::cache::BlobCache::default_root()
        } else {
            std::path::PathBuf::from(&self.cache_dir)
        }
    }

    /// Delete cached file contents, keeping anything pinned or still arriving. Returns
    /// bytes freed.
    pub fn cache_clear(&self) -> u64 {
        let Ok(cache) = mouse_clipboard::cache::BlobCache::new(self.cache_root()) else {
            return 0;
        };
        // Same protection eviction uses: pinned entries and in-flight transfers are not
        // the program's to throw away, whoever pressed the button.
        cache.clear_except(&self.clip_history.referenced_hashes())
    }

    /// Stop an in-flight transfer. What has arrived stays on disk, so clicking the
    /// entry again resumes from there.
    pub fn clipboard_cancel(&self, id: u64) {
        self.clip_commands.cancel(id);
    }

    /// Answer "not now" — quieten the prompt without discarding the entry.
    pub fn clipboard_decline(&self, id: u64) {
        self.clip_commands.decline(id);
    }

    /// Raise the "ask first" threshold on the running engine, so "always allow this
    /// size" takes effect on the next file rather than only after a restart.
    pub fn clipboard_set_auto_max(&self, bytes: u64) {
        self.clip_commands.set_auto_max(bytes);
    }

    /// Pin or unpin an entry. Returns the new state.
    pub fn clipboard_toggle_pin(&self, id: u64) -> Option<bool> {
        self.clip_history.toggle_pin(id)
    }

    pub fn clipboard_remove(&self, id: u64) {
        self.clip_history.remove(id);
    }

    /// Measured bulk throughput, so the panel can say what the quick-send rule is
    /// actually working from rather than implying a number we do not have.
    pub fn measured_throughput(&self) -> Option<u64> {
        self.health.throughput_bps()
    }

    /// Drop every entry the user has not pinned.
    pub fn clipboard_clear(&self) {
        self.clip_history.clear();
    }
}

#[cfg(feature = "gui")]
impl Drop for EngineHandle {
    fn drop(&mut self) {
        // Written on the way out rather than on every copy: this is clipboard content,
        // and rewriting it per keystroke would both cost I/O and widen the window in
        // which a copied password sits in a file.
        {
            let cache = mouse_clipboard::cache::BlobCache::new(if self.cache_dir.trim().is_empty() {
                mouse_clipboard::cache::BlobCache::default_root()
            } else {
                std::path::PathBuf::from(&self.cache_dir)
            })
            .ok();
            if let Err(e) =
                self.clip_history.save(&history_path(), cache.as_ref(), self.persist)
            {
                tracing::warn!("could not save clipboard history: {e}");
            }
        }
        self.cancel.cancel();
        if let Some(t) = self.thread.take() {
            // The engine's own teardown is bounded (≤ ~2s), so this join is bounded too.
            let _ = t.join();
        }
    }
}

/// Detect the live screen size and reconcile it with config.
/// Pick the injection backend for the session we are running in.
fn injector_for(
    session: Session,
    dims: (i32, i32),
) -> anyhow::Result<Box<dyn mouse_input::InputInjector>> {
    match session {
        Session::Desktop => Ok(new_injector()?),
        #[cfg(target_os = "linux")]
        Session::Greeter => Ok(mouse_input::new_sessionless_injector(dims.0, dims.1)?),
        // Nothing but Linux has a greeter in this sense; the caller never builds one.
        #[cfg(not(target_os = "linux"))]
        Session::Greeter => {
            let _ = dims;
            Ok(new_injector()?)
        }
    }
}

pub fn resolve_dims(cfg: &mut Config) -> (i32, i32) {
    match screen_size() {
        Ok((w, h)) => {
            if (w, h) != (cfg.screen_width, cfg.screen_height) {
                tracing::warn!(
                    "config screen {}x{} differs from detected {w}x{h}; using detected",
                    cfg.screen_width,
                    cfg.screen_height
                );
            }
            cfg.screen_width = w;
            cfg.screen_height = h;
            (w, h)
        }
        Err(e) => {
            tracing::warn!(
                "could not detect screen size ({e}); using config {}x{}",
                cfg.screen_width,
                cfg.screen_height
            );
            (cfg.screen_width, cfg.screen_height)
        }
    }
}

/// Spawn the clipboard-sync thread if enabled, returning the sender the engine uses
/// to route received `Clipboard` messages into it, plus the handle kept for teardown.
fn setup_clipboard(
    cfg: &Config,
    to_peer: &tokio::sync::mpsc::UnboundedSender<Message>,
    history: Option<ClipboardHistory>,
    commands: Option<ClipCommandRx>,
    bulk: Option<mouse_clipboard::BulkLink>,
) -> (Option<ClipboardRoute>, Option<ClipboardSync>) {
    if !cfg.clipboard_sync {
        return (None, None);
    }
    // Apply the user's limits to the history the panel already holds, so a settings
    // change takes effect on the next Start without discarding what is in it.
    if let Some(h) = &history {
        h.set_limits(clipboard_limits(cfg));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sync = ClipboardSync::spawn(
        to_peer.clone(),
        rx,
        commands,
        connected.clone(),
        history,
        clipboard_settings(cfg),
        bulk,
    );
    (Some(ClipboardRoute { tx, connected }), Some(sync))
}

/// Which port the bulk lane uses. `0` means "the same one the input link is on", so
/// the default deployment needs nothing extra opened in a firewall — the lane is
/// declared in the handshake instead of being implied by the port.
fn bulk_port(cfg: &Config) -> u16 {
    match cfg.bulk_port {
        0 => match cfg.role {
            Role::Server => cfg.listen_port,
            Role::Client => cfg.server_port,
        },
        p => p,
    }
}

/// Everything the clipboard thread needs to use the bulk lane.
struct BulkWiring {
    channels: Option<BulkChannels>,
    link: Option<mouse_clipboard::BulkLink>,
}

/// Build both halves of the bulk lane and the blob cache behind it.
///
/// The cache is opened here rather than in the clipboard thread so a failure — a
/// read-only home, a full disk — is reported once at startup and simply leaves file
/// transfer switched off, instead of failing on every copy.
fn bulk_settings(cfg: &Config) -> BulkSettings {
    BulkSettings {
        pause_while_crossed: cfg.bulk_pause_while_crossed,
        // 0 means "adapt", which is what protects the mouse automatically.
        rate_cap: match cfg.bulk_rate_limit_mbps {
            0 => None,
            m => Some(m as u64 * 1024 * 1024),
        },
    }
}

/// Where the saved clipboard history lives.
pub fn history_path() -> std::path::PathBuf {
    Config::default_path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("history.json")
}

fn setup_bulk(cfg: &Config, health: std::sync::Arc<LinkHealth>) -> BulkWiring {
    if !cfg.bulk_enabled {
        return BulkWiring { channels: None, link: None };
    }
    let root = if cfg.cache_dir.trim().is_empty() {
        mouse_clipboard::cache::BlobCache::default_root()
    } else {
        std::path::PathBuf::from(&cfg.cache_dir)
    };
    let cache = match mouse_clipboard::cache::BlobCache::new(root.clone()) {
        Ok(c) => std::sync::Arc::new(c),
        Err(e) => {
            tracing::warn!(path = %root.display(), "no file cache ({e}); file copying is off");
            return BulkWiring { channels: None, link: None };
        }
    };
    // outgoing: clipboard thread -> network. incoming: network -> clipboard thread.
    let (out_tx, out_rx) = unbounded_channel();
    let (in_tx, in_rx) = std::sync::mpsc::channel();
    // One counter, shared both ways: the sender adds to it, the lane subtracts as bytes
    // actually leave. This is what stops a 4 GB file being read into memory far faster
    // than the network can drain it.
    let queue = std::sync::Arc::new(mouse_core::bulk::BulkQueue::new(
        u64::from(cfg.bulk_send_buffer_mb.max(1)) * 1024 * 1024,
    ));
    BulkWiring {
        channels: Some(BulkChannels {
            outgoing: out_rx,
            incoming: in_tx,
            queue: queue.clone(),
        }),
        link: Some(mouse_clipboard::BulkLink {
            to_bulk: out_tx,
            from_bulk: in_rx,
            cache,
            health,
            queue,
        }),
    }
}

/// Keep the client's bulk lane up, retrying quietly on its own schedule.
///
/// Deliberately separate from the input link's reconnect policy: the bulk lane must
/// never influence whether input is considered connected, and its failures are not
/// something the user should have to see or act on.
async fn run_bulk_client_forever(
    cfg: Config,
    params: LinkParams,
    cancel: CancellationToken,
    ch: &mut BulkChannels,
    health: std::sync::Arc<LinkHealth>,
) {
    let port = bulk_port(&cfg);
    let settings = bulk_settings(&cfg);
    let mut backoff = params.reconnect_initial;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match mouse_core::bulk::run_bulk_client(
            &cfg,
            port,
            params.handshake_timeout,
            &cancel,
            ch,
            &health,
            settings,
        )
        .await
        {
            Ok(()) => backoff = params.reconnect_initial,
            Err(e) => tracing::debug!("bulk lane not available ({e}); will retry"),
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(params.reconnect_max);
    }
}

/// The user's history limits, straight from config.
pub fn clipboard_limits(cfg: &Config) -> HistoryLimits {
    HistoryLimits {
        max_items: cfg.clipboard_history_size.max(1),
        max_item_bytes: cfg.clipboard_max_item_bytes,
        memory_budget: cfg.clipboard_memory_budget,
    }
}

/// The user's sync settings, straight from config.
fn clipboard_settings(cfg: &Config) -> ClipboardSettings {
    ClipboardSettings {
        sync_text: cfg.clipboard_sync_text,
        sync_images: cfg.clipboard_sync_images,
        max_item_bytes: cfg.clipboard_max_item_bytes,
        max_image_px: cfg.clipboard_max_image_px.max(1),
        sync_files: cfg.clipboard_sync_files,
        // 0 means "always ask"; the panel says so next to the field.
        auto_transfer_max: cfg.clipboard_auto_transfer_max,
        cache_budget: cfg.clipboard_cache_budget,
        // Leaked once at startup so the clipboard thread can hold a &'static str for
        // the life of the process; the alternative is threading a String through every
        // frame for a value that never changes.
        machine: Box::leak(cfg.name.clone().into_boxed_str()),
        inline_image_max: cfg.clipboard_inline_image_max.max(1),
        max_files_per_copy: cfg.clipboard_max_files_per_copy.max(1),
        follow_symlinks: cfg.clipboard_follow_symlinks,
        images_also_as_file: cfg.clipboard_images_also_as_file,
        move_on_paste: cfg.clipboard_move_on_paste,
        hash_limit: cfg.clipboard_hash_limit,
        deliver_dir: {
            let d = cfg.clipboard_deliver_dir.trim();
            (!d.is_empty()).then(|| std::path::PathBuf::from(d))
        },
        quick_send_secs: cfg.clipboard_quick_send_secs as f64,
        persist_history: cfg.clipboard_persist_history,
    }
}

/// Cancel the token on Ctrl-C so a Stop request tears everything down promptly.
fn spawn_ctrl_c(cancel: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl-C received; stopping");
            cancel.cancel();
        }
    });
}
