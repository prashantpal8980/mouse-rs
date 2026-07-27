//! Engine lifecycle: build the network link + edge-crossing engine for a role and
//! run it until cancelled. Shared by the headless CLI (`main`) and the GUI's
//! Start/Stop button so both drive the **exact same** startup and teardown path —
//! the lifecycle that guarantees Stop never hangs and input is never left stuck.

use std::time::Duration;

use mouse_clipboard::{ClipboardHistory, ClipboardSync};
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
                setup_clipboard(&cfg, &engine_link.to_peer, clip_history.clone());

            let cfg_link = cfg.clone();
            let cfg_engine = cfg.clone();
            let c1 = cancel.clone();
            let c2 = cancel.clone();
            runtime.block_on(async move {
                if install_ctrl_c {
                    spawn_ctrl_c(cancel.clone());
                }
                let mut link = tokio::spawn(run_server(cfg_link, params, c1, link_channels));
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
                setup_clipboard(&cfg, &engine_link.to_peer, clip_history.clone());

            let cfg_link = cfg.clone();
            let cfg_engine = cfg.clone();
            let c1 = cancel.clone();
            let c2 = cancel.clone();
            runtime.block_on(async move {
                if install_ctrl_c {
                    spawn_ctrl_c(cancel.clone());
                }
                let mut link = tokio::spawn(run_client(cfg_link, params, c1, link_channels));
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
}

#[cfg(feature = "gui")]
impl EngineHandle {
    /// Start the engine for `cfg.role` on a background thread. Returns immediately.
    pub fn start(cfg: Config) -> EngineHandle {
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let status = StatusHandle::new();
        let s = status.clone();
        let clip_history = ClipboardHistory::new();
        let ch = clip_history.clone();
        let thread = std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || {
                if let Err(e) = run_blocking(cfg, c, false, Some(s), Some(ch)) {
                    tracing::error!("engine exited with error: {e}");
                }
            })
            .expect("spawn engine thread");
        EngineHandle {
            cancel,
            thread: Some(thread),
            status,
            clip_history,
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

    /// The shared clipboard-transfer history (newest first) for the UI.
    pub fn clipboard_history(&self) -> Vec<mouse_clipboard::ClipEntry> {
        self.clip_history.snapshot()
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
) -> (Option<ClipboardRoute>, Option<ClipboardSync>) {
    if !cfg.clipboard_sync {
        return (None, None);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sync = ClipboardSync::spawn(to_peer.clone(), rx, connected.clone(), history);
    (Some(ClipboardRoute { tx, connected }), Some(sync))
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
