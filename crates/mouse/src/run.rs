//! Engine lifecycle: build the network link + edge-crossing engine for a role and
//! run it until cancelled. Shared by the headless CLI (`main`) and the GUI's
//! Start/Stop button so both drive the **exact same** startup and teardown path —
//! the lifecycle that guarantees Stop never hangs and input is never left stuck.

use std::time::Duration;

use mouse_clipboard::{ClipCommandRx, ClipboardHistory, ClipboardSettings, ClipboardSync, HistoryLimits};
use mouse_core::bulk::{BulkChannels, BulkServer, LinkHealth};
use mouse_core::net::{run_client, run_server, LinkParams};
use mouse_core::protocol::Message;
use mouse_core::{Config, Role};
use mouse_engine::{
    run_client_engine, run_server_engine, ClipboardRoute, EngineLink, Injector, StatusHandle,
};
use mouse_input::{new_injector, screen_size, start_capture, NamedKey};
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

/// Run the engine for `cfg.role` on the **current thread** until `cancel` fires.
/// Blocks until the link/engine tasks finish and the input backends are torn down.
///
/// `install_ctrl_c` wires Ctrl-C → `cancel` (the headless CLI uses it; the GUI owns
/// its own Stop button and passes `false`).
pub fn run_blocking(
    mut cfg: Config,
    cancel: CancellationToken,
    install_ctrl_c: bool,
    status: Option<StatusHandle>,
    clip_history: Option<ClipboardHistory>,
    clip_commands: Option<ClipCommandRx>,
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
    let health = LinkHealth::new();
    let (link_channels, mut engine_link) = EngineLink::new();
    // Wire the UI's status sink in (the headless CLI passes `None`).
    engine_link.status = status;

    match cfg.role {
        Role::Server => {
            let (cap_tx, cap_rx) = unbounded_channel();
            // Pass the configured lock/return hotkey so the Windows capture failsafe
            // can self-release suppression on it (the Linux backend ignores it).
            let (capture, suppressor) =
                start_capture(cap_tx, NamedKey::from_name(&cfg.lock_hotkey))?;
            let (clip_route, clipboard) =
                setup_clipboard(&cfg, &engine_link.to_peer, clip_history.clone(), clip_commands);

            let cfg_link = cfg.clone();
            let cfg_engine = cfg.clone();
            let c1 = cancel.clone();
            let c2 = cancel.clone();
            // With a separate bulk port the shared listener refuses bulk connections
            // and a second listener serves them instead; with the default (0) the two
            // lanes share one port and one listener.
            let separate_bulk_port = bulk_port(&cfg) != cfg.listen_port;
            let (bulk_server, _bulk_rx) = setup_bulk_server(&cfg, health.clone());
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
            let injector = Injector::spawn(new_injector()?)?;
            let inject_tx = injector.sender();
            let (clip_route, clipboard) =
                setup_clipboard(&cfg, &engine_link.to_peer, clip_history.clone(), clip_commands);

            let cfg_link = cfg.clone();
            let cfg_engine = cfg.clone();
            let cfg_bulk_client = cfg.clone();
            let c1 = cancel.clone();
            let c2 = cancel.clone();
            let (bulk_channels, _bulk_tx) = setup_bulk_client(&cfg);
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
}

#[cfg(feature = "gui")]
impl EngineHandle {
    /// Start the engine for `cfg.role` on a background thread. Returns immediately.
    pub fn start(cfg: Config) -> EngineHandle {
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let status = StatusHandle::new();
        let s = status.clone();
        let clip_history = ClipboardHistory::with_limits(clipboard_limits(&cfg));
        let ch = clip_history.clone();
        // The panel keeps the sending half so it can ask for an entry to be put back on
        // the clipboard; the receiving half goes to the clipboard thread, which is the
        // only thread allowed to touch the OS clipboard.
        let (clip_commands, clip_command_rx) = mouse_clipboard::command_channel();
        let thread = std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || {
                if let Err(e) = run_blocking(cfg, c, false, Some(s), Some(ch), Some(clip_command_rx))
                {
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

    /// Pin or unpin an entry. Returns the new state.
    pub fn clipboard_toggle_pin(&self, id: u64) -> Option<bool> {
        self.clip_history.toggle_pin(id)
    }

    pub fn clipboard_remove(&self, id: u64) {
        self.clip_history.remove(id);
    }

    /// Drop every entry the user has not pinned.
    pub fn clipboard_clear(&self) {
        self.clip_history.clear();
    }
}

#[cfg(feature = "gui")]
impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(t) = self.thread.take() {
            // The engine's own teardown is bounded (≤ ~2s), so this join is bounded too.
            let _ = t.join();
        }
    }
}

/// Detect the live screen size and reconcile it with config.
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

/// Server side of the bulk lane: the handle the input listener hands connections to.
fn setup_bulk_server(
    cfg: &Config,
    health: std::sync::Arc<LinkHealth>,
) -> (
    BulkServer,
    Option<tokio::sync::mpsc::UnboundedReceiver<mouse_core::bulk::BulkFrame>>,
) {
    if !cfg.bulk_enabled {
        return (BulkServer::disabled(), None);
    }
    let (in_tx, in_rx) = unbounded_channel();
    let (_out_tx, out_rx) = unbounded_channel();
    (
        BulkServer::enabled(
            BulkChannels {
                outgoing: out_rx,
                incoming: in_tx,
            },
            health,
        ),
        Some(in_rx),
    )
}

/// Client side of the bulk lane.
fn setup_bulk_client(
    cfg: &Config,
) -> (
    Option<BulkChannels>,
    Option<tokio::sync::mpsc::UnboundedSender<mouse_core::bulk::BulkFrame>>,
) {
    if !cfg.bulk_enabled {
        return (None, None);
    }
    let (in_tx, _in_rx) = unbounded_channel();
    let (out_tx, out_rx) = unbounded_channel();
    (
        Some(BulkChannels {
            outgoing: out_rx,
            incoming: in_tx,
        }),
        Some(out_tx),
    )
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
