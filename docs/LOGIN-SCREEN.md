# Sharing input at the Linux login screen

Autostart runs mouse-rs when the desktop session starts, which is *after* the display
manager has taken a password. That is why logging in needed a second keyboard plugged
into the machine — the one moment the shared keyboard was unavailable was the moment it
was most wanted.

This mode closes that gap.

## Why the normal backend cannot do it

Linux injection used XTEST, which talks to **one specific X server** through `DISPLAY`
and an auth cookie. At the greeter:

* the user's X server does not exist yet, and
* the greeter's own belongs to a different account (`sddm`) with its own cookie.

There is nothing for mouse-rs to inject into, and no amount of autostart timing changes
that. The fix has to go underneath the session rather than earlier in it.

## What it does instead

`/dev/uinput` lets a privileged process ask the kernel for a virtual keyboard and
pointer. Events written to them are indistinguishable from a real USB device's, and the
kernel delivers them to whatever is in front — the greeter, a desktop session, a text
console, or a Wayland compositor. Nothing session-specific is involved, so nothing
session-specific can break.

Two devices, not one: the kernel classifies a device by what it can emit, and a node
advertising both keys and absolute axes gets treated as a tablet by libinput.

The pointer is **absolute-only**. A relative virtual device is subject to this machine's
pointer acceleration curve, so a 100-pixel movement on the controlling machine would
land somewhere else here. Driving absolute axes we compute ourselves makes remote motion
mean the same thing on both machines.

## What it costs

Two things, both inherent rather than incidental:

* **Privilege.** `/dev/uinput` is `root:root 0600` on a stock system. Creating a virtual
  input device is exactly the capability a keylogger wants, so there is no unprivileged
  path to it — the service runs as root, hardened down to the network, `/etc/mouse-rs`,
  its own state directory and that one device.
* **It is a network-driven keystroke injector running before anyone has logged in.** A
  peer holding `shared_secret` can type at the login prompt. That is the feature, not a
  flaw in it — but it does mean the secret is now what stands between the LAN and this
  machine's login. It is off unless you install it deliberately, and the config holding
  that secret is root-only.

One more, worth knowing before it surprises you: uinput emits **physical key positions**,
and the active keymap decides what they produce. `a` becomes `KEY_A`, which types `a`
under a US layout. A layout that moves the A key types something else. Every KVM that
injects at this level makes the same trade.

## Turning it on

**From the panel.** Startup tab → **Share input at the login screen**. The first time it
is switched on, a one-time panel explains what it costs and asks to confirm; after that
the toggle acts immediately. Either answer is remembered, so the explanation never
appears twice.

Setting up a system service needs administrator rights, which the panel does not have —
so it asks `pkexec` to do it, and your desktop's own authentication agent prompts for the
password. That password goes to polkit, never through mouse-rs.

The toggle reads its state back from systemd rather than from a saved setting, so it also
reflects a change made with `systemctl` outside the app, and it does **not** move if the
password prompt is dismissed. A toggle that moved anyway would claim the login screen was
covered when it was not — and the way you would find out is by being locked out with no
keyboard.

Note that the toggle shows ON while the service is installed and enabled, even though it
is *stopped* for as long as you are logged in. That is the hand-over working, not a fault.

**From a terminal**, if you prefer, or if `pkexec` is unavailable:

```bash
mouse-rs greeter-status      # what it is currently doing; needs no privilege
```

The machine that receives input is the one that needs this — here, the Kali desktop.
Pair the two machines normally first, so a shared secret exists.

```bash
# 1. Prove the virtual devices work, before trusting them somewhere with no terminal.
#    Focus a text editor when it asks; it types `mouse-rs ok`.
sudo ./target/release/mouse-rs greeter-selftest

# 2. Install. Copies the binary to /usr/local/bin, writes a root-only config from
#    yours, installs the unit and enables it.
sudo ./target/release/mouse-rs greeter-install

# 3. Start it now rather than waiting for a reboot.
sudo systemctl start mouse-rs-greeter
```

Then log out. The login screen should accept the peer's keyboard.

```bash
systemctl status mouse-rs-greeter
journalctl -u mouse-rs-greeter -n 50
```

The service's own log goes to `/var/lib/mouse-rs/.mouse_share/mouse-rs.log` and is
shipped like any other, so it can be read from the other machine.

To remove everything it installed:

```bash
sudo mouse-rs greeter-uninstall
```

## The hand-over

Once you log in, your own instance starts and connects. Two clients injecting the same
stream means every keystroke arrives twice, so exactly one may run: the service stops the
moment logind reports an active graphical session, and `Restart=always` brings it back
when that session ends — which is when the login screen returns.

The check deliberately ignores logind's `CLASS=greeter` session (the login screen itself)
and `CLASS=manager` (the per-user manager systemd keeps alongside a real session). Both
are active sessions; neither is a seat anyone is typing at. Matching the first would make
the service stop the instant it started, which looks exactly like the feature not working.

## Re-run after changing settings

The config in `/etc/mouse-rs/config.json` is a **copy**. It does not follow later edits
to your own — so after changing the shared secret or the peer's address, re-run
`greeter-install`.

## Not offered: greeter mode as a server

This mode is always a client. Serving would mean capturing *this* machine's keyboard
before anyone has logged in, which is a different and much larger capability than
injecting into it, and the setup here has the physical keyboard on the other machine
anyway.
