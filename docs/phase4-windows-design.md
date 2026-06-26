# Phase 4 — Windows input backend design

Synthesized from a research+design workflow (2026-06-26). Mirrors `linux.rs` so the
async engine drives it unchanged. Implements `InputInjector` + `InputCapture` for
`x86_64-pc-windows-gnu`.

## Verified Win32 facts that drive the design
1. **Echo absorption via `dwExtraInfo`, not `LLMHF_INJECTED`.** `LLMHF_INJECTED`
   fires for *any* process's injection (other KVMs, AHK, RDP), so it false-positives.
   The supported way to recognize *our own* events is to stamp `dwExtraInfo` with a
   magic constant on every `SendInput`, and match it in the hook. `SetCursorPos`
   can't carry `dwExtraInfo` → **never use `SetCursorPos`; warp via `SendInput`**.
2. **LL-hook timeout** (`HKCU\Control Panel\Desktop\LowLevelHooksTimeout`, default
   300 ms): a slow hook proc is bypassed per-event, and after repeated timeouts the
   OS **silently removes the hook** (Win7+). ⇒ the hook proc MUST be O(1),
   non-blocking — this is the structural fix for the Python "stuck hook" bug. It also
   means a wedged pump can't *freeze* input (OS removes the hook), only break the KVM.
3. **Cross-thread `UnhookWindowsHookEx` is NOT confirmed safe** → the watchdog does
   not unhook cross-thread. It only sets `suppress=false` + `PostThreadMessage(WM_QUIT)`;
   the installing (pump) thread unhooks on its way out. OS auto-removal (fact 2) covers
   a truly wedged pump, so no freeze.

## Architecture (windows.rs, top-to-bottom, parallel to linux.rs)
- `INJECT_MAGIC: usize` stamped into `dwExtraInfo` of every injected event.
- `ensure_dpi_aware()` behind a `Once` → `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`.
  Called first in every public entry point, before any cursor/input/metric call.
- Key map: `named_to_vk`, `vk_to_named`, `is_extended_vk`, `char_to_vk` (VkKeyScanW),
  `vk_to_basechar` (MapVirtualKeyW MAPVK_VK_TO_CHAR), `vk_to_key`. `VK_RMENU↔AltGr`.
- `Motion`/`MotionGeom` — shared with linux via `src/motion.rs`.
- `WinInjector` (zero-field) → `SendInput` choke point. `mouse_move_rel` =
  GetCursorPos+add+clamp+**absolute** SendInput (dodges pointer ballistics for 1:1).
  Abs normalized over the **virtual desktop** to 0..65535 with `MOUSEEVENTF_VIRTUALDESK`.
  Scroll `*120` clamped to 32. Keys: VK+scancode, `KEYEVENTF_EXTENDEDKEY` for the
  extended set, `KEYEVENTF_UNICODE` fallback (surrogate pairs) when `VkKeyScanW`==-1.
- `WinCapture`: one dedicated "win-capture" thread installs `WH_MOUSE_LL` +
  `WH_KEYBOARD_LL` and runs a `MsgWaitForMultipleObjectsEx(8ms)` + `PeekMessage` pump.
  Hook state lives in a `thread_local! RefCell<Option<HookState>>` (procs run on the
  pump thread). Procs are O(1)/non-blocking/panic-free (use `try_borrow_mut`).
  - mouse_proc: `dwExtraInfo==MAGIC` ⇒ our echo (rebase Motion to centre, swallow if
    grabbed). Grabbed ⇒ `Motion::on_motion` → forward MouseMove; near-edge ⇒ warp via
    SendInput(MAGIC); buttons/wheel forwarded; **return 1 (swallow)**. Not grabbed ⇒
    forward `Cursor{x,y}` (deduped) + `CallNextHookEx` (replaces X11's poll).
  - keyboard_proc: side-specific VKs give L/R fidelity. Grabbed ⇒ map vk(+extended)→Key;
    if it's the return hotkey on key-down, **flip suppress=false in-proc** (the Fn+Up
    escape, no network dependency) and still forward; forward Input; return 1. Not
    grabbed ⇒ CallNextHookEx. Phantom-LCtrl-before-RMENU filter for AltGr.
  - reconcile (per pump tick): rising edge ⇒ grabbed=true, warp to centre, held-modifier
    snapshot via `GetAsyncKeyState` on the 8 side-specific modifier VKs. Falling ⇒ grabbed=false.
    No exclusive grab exists on Windows, so `SuppressRejected` is emitted only if hook
    install fails.
- Failsafe = 4 layers: (1) non-blocking procs, (2) OS timeout/auto-removal, (3) watchdog
  (suppress=false + WM_QUIT on stale heartbeat — no cross-thread unhook), (4) return
  hotkey self-release in-proc.
- Teardown: `stop` flag + `PostThreadMessage(WM_QUIT)` + `join_bounded(500ms)`; pump
  also polls `stop` each tick. Unhook + clear thread-local on the way out.

## Windows-box test plan (the user runs these on the laptop)
Fn+Up freeze regression; Ctrl+C/V composing across the wire; held-modifier-at-crossing;
warp-echo absorption (no spurious jumps); 3rd-party-injection isolation (AHK move not
absorbed); return-hotkey escape with the peer killed; DPI 150% fidelity; rel-motion 1:1
with Enhance-Pointer-Precision on/off; L/R modifier + extended-key fidelity; scroll
direction/magnitude; teardown never hangs; AltGr Level3 + phantom-LCtrl filtered.
