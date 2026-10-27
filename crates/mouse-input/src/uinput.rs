//! Session-independent injection via the kernel's `uinput` device.
//!
//! The X11 backend in [`crate::linux`] injects with XTEST, which means it talks to one
//! specific X server through `DISPLAY` and an auth cookie. That is fine inside a desktop
//! session and useless outside one: at the display-manager greeter the user's X server
//! does not exist yet, and the greeter's own belongs to a different account entirely.
//! So the pointer and keyboard cannot cross until after someone has already typed their
//! password — which is the one moment they would most like not to need a second keyboard.
//!
//! `uinput` sidesteps the problem by going underneath it. We ask the kernel to create a
//! virtual keyboard and a virtual pointer; from then on the events we write are
//! indistinguishable from a real USB device's, and the kernel hands them to whatever is
//! in front — the greeter, a desktop session, a text console, or a Wayland compositor.
//! Nothing session-specific is involved, so nothing session-specific can break.
//!
//! What it costs:
//!
//! * **Privilege.** `/dev/uinput` is `root:root 0600` on a stock system. Either run as
//!   root or grant the device to a group with a udev rule. There is no unprivileged path
//!   — creating a virtual input device is exactly the capability a keylogger would want.
//! * **Keycodes, not characters.** We emit *physical key positions* and the active keymap
//!   decides what they produce. `Key::Char('a')` becomes `KEY_A`, which types `a` under a
//!   US layout and something else under a layout that moves the A key. This is inherent
//!   to injecting at this level and is the same trade every KVM of this kind makes.
//!
//! Two devices rather than one, because the kernel classifies a device by what it can
//! emit: a node advertising both keys and absolute axes gets treated as a tablet by
//! libinput and behaves oddly. Keyboard and pointer stay separate.
//!
//! The pointer is **absolute-only**. Relative deltas are tracked here and emitted as
//! absolute positions, which is deliberate: a relative virtual device is subject to the
//! local pointer acceleration curve, so a 100-pixel movement on the controlling machine
//! would land somewhere else entirely on this one. Driving absolute axes we compute
//! ourselves makes remote motion mean the same thing on both machines.
//!
//! Only x86_64/aarch64 Linux is targeted, where the `asm-generic` ioctl encoding applies.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;

use mouse_core::protocol::Button;

use crate::keys::{Key, NamedKey};
use crate::{InputError, InputInjector};

// ----------------------------------------------------------------- kernel ABI

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0;

const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
/// `ABS_CNT` from `linux/input-event-codes.h`: the length of every per-axis array in
/// [`UinputUserDev`]. Wrong here means a struct the kernel reads past the end of.
const ABS_CNT: usize = 0x40;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

const BUS_VIRTUAL: u16 = 0x06;
const UINPUT_MAX_NAME_SIZE: usize = 80;

/// `_IO(ty, nr)` — no argument.
const fn io(ty: u8, nr: u8) -> libc::c_ulong {
    ((ty as libc::c_ulong) << 8) | nr as libc::c_ulong
}

/// `_IOW(ty, nr, size)` — write direction, `size` bytes of argument.
///
/// Encoded rather than hardcoded so the numbers are checkable against the header
/// instead of being four magic constants nobody can verify.
const fn iow(ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    (1 << 30)
        | ((size as libc::c_ulong) << 16)
        | ((ty as libc::c_ulong) << 8)
        | nr as libc::c_ulong
}

const UI_DEV_CREATE: libc::c_ulong = io(b'U', 1);
const UI_DEV_DESTROY: libc::c_ulong = io(b'U', 2);
const UI_SET_EVBIT: libc::c_ulong = iow(b'U', 100, 4);
const UI_SET_KEYBIT: libc::c_ulong = iow(b'U', 101, 4);
const UI_SET_RELBIT: libc::c_ulong = iow(b'U', 102, 4);
const UI_SET_ABSBIT: libc::c_ulong = iow(b'U', 103, 4);

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct InputEventRaw {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
struct UinputUserDev {
    name: [libc::c_char; UINPUT_MAX_NAME_SIZE],
    id: InputId,
    ff_effects_max: u32,
    absmax: [i32; ABS_CNT],
    absmin: [i32; ABS_CNT],
    absfuzz: [i32; ABS_CNT],
    absflat: [i32; ABS_CNT],
}

// --------------------------------------------------------------- key mapping

/// Linux keycode for a named key.
///
/// `AltGr` and `AltR` both resolve to `KEY_RIGHTALT`, which is not a shortcut: on Linux
/// AltGr *is* the right Alt position, and whether it composes characters is a property of
/// the keymap rather than of the key.
fn named_code(k: NamedKey) -> Option<u16> {
    Some(match k {
        NamedKey::Escape => 1,
        NamedKey::Backspace => 14,
        NamedKey::Tab => 15,
        NamedKey::Enter => 28,
        NamedKey::CtrlL => 29,
        NamedKey::ShiftL => 42,
        NamedKey::ShiftR => 54,
        NamedKey::AltL => 56,
        NamedKey::Space => 57,
        NamedKey::CapsLock => 58,
        NamedKey::NumLock => 69,
        NamedKey::ScrollLock => 70,
        NamedKey::CtrlR => 97,
        NamedKey::PrintScreen => 99, // KEY_SYSRQ
        NamedKey::AltR | NamedKey::AltGr => 100,
        NamedKey::Home => 102,
        NamedKey::Up => 103,
        NamedKey::PageUp => 104,
        NamedKey::Left => 105,
        NamedKey::Right => 106,
        NamedKey::End => 107,
        NamedKey::Down => 108,
        NamedKey::PageDown => 109,
        NamedKey::Insert => 110,
        NamedKey::Delete => 111,
        NamedKey::Pause => 119,
        NamedKey::SuperL => 125,
        NamedKey::SuperR => 126,
        NamedKey::Menu => 127, // KEY_COMPOSE
        // F1-F10 are contiguous, then the block was full: F11/F12 sit apart, and
        // F13-F24 were added later still, somewhere else again.
        NamedKey::F(n) => match n {
            1..=10 => 58 + n as u16,
            11 => 87,
            12 => 88,
            13..=24 => 183 + (n as u16 - 13),
            _ => return None,
        },
    })
}

/// Linux keycode for the base character of a printable key, US layout.
fn char_code(c: char) -> Option<u16> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        '1' => 2,
        '2' => 3,
        '3' => 4,
        '4' => 5,
        '5' => 6,
        '6' => 7,
        '7' => 8,
        '8' => 9,
        '9' => 10,
        '0' => 11,
        '-' => 12,
        '=' => 13,
        'q' => 16,
        'w' => 17,
        'e' => 18,
        'r' => 19,
        't' => 20,
        'y' => 21,
        'u' => 22,
        'i' => 23,
        'o' => 24,
        'p' => 25,
        '[' => 26,
        ']' => 27,
        'a' => 30,
        's' => 31,
        'd' => 32,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        ';' => 39,
        '\'' => 40,
        '`' => 41,
        '\\' => 43,
        'z' => 44,
        'x' => 45,
        'c' => 46,
        'v' => 47,
        'b' => 48,
        'n' => 49,
        'm' => 50,
        ',' => 51,
        '.' => 52,
        '/' => 53,
        ' ' => 57,
        _ => return None,
    })
}

/// Every keycode the keyboard device must declare up-front.
///
/// A device may only emit codes it registered before creation, so this has to be the
/// union of everything [`named_code`] and [`char_code`] can return. Built by asking
/// them rather than by repeating the list, so the two cannot drift apart.
fn all_key_codes() -> Vec<u16> {
    let named = [
        NamedKey::Enter, NamedKey::Escape, NamedKey::Backspace, NamedKey::Tab,
        NamedKey::Space, NamedKey::Insert, NamedKey::Delete, NamedKey::Up,
        NamedKey::Down, NamedKey::Left, NamedKey::Right, NamedKey::Home,
        NamedKey::End, NamedKey::PageUp, NamedKey::PageDown, NamedKey::CapsLock,
        NamedKey::NumLock, NamedKey::ScrollLock, NamedKey::PrintScreen,
        NamedKey::Pause, NamedKey::Menu, NamedKey::ShiftL, NamedKey::ShiftR,
        NamedKey::CtrlL, NamedKey::CtrlR, NamedKey::AltL, NamedKey::AltR,
        NamedKey::AltGr, NamedKey::SuperL, NamedKey::SuperR,
    ];
    let mut codes: Vec<u16> = named.iter().filter_map(|k| named_code(*k)).collect();
    codes.extend((1u8..=24).filter_map(|n| named_code(NamedKey::F(n))));
    let printable = "abcdefghijklmnopqrstuvwxyz0123456789-=[];'`\\,./ ";
    codes.extend(printable.chars().filter_map(char_code));
    codes.sort_unstable();
    codes.dedup();
    codes
}

// -------------------------------------------------------------------- device

/// One created uinput node, destroyed on drop.
struct Device {
    file: File,
}

impl Device {
    /// Open `/dev/uinput`, declare `evbits`/`keys`/`rels`/`abs`, and create the node.
    fn create(
        name: &str,
        keys: &[u16],
        rels: &[u16],
        abs: &[(u16, i32, i32)],
    ) -> io::Result<Device> {
        let file = OpenOptions::new().write(true).open("/dev/uinput")?;
        let fd = file.as_raw_fd();

        if !keys.is_empty() {
            set_bit(fd, UI_SET_EVBIT, EV_KEY as i32)?;
            for k in keys {
                set_bit(fd, UI_SET_KEYBIT, *k as i32)?;
            }
        }
        if !rels.is_empty() {
            set_bit(fd, UI_SET_EVBIT, EV_REL as i32)?;
            for r in rels {
                set_bit(fd, UI_SET_RELBIT, *r as i32)?;
            }
        }
        if !abs.is_empty() {
            set_bit(fd, UI_SET_EVBIT, EV_ABS as i32)?;
            for (a, _, _) in abs {
                set_bit(fd, UI_SET_ABSBIT, *a as i32)?;
            }
        }
        // EV_SYN is implicit for every device but declaring it is free and explicit.
        set_bit(fd, UI_SET_EVBIT, EV_SYN as i32)?;

        let mut dev = UinputUserDev {
            name: [0; UINPUT_MAX_NAME_SIZE],
            id: InputId { bustype: BUS_VIRTUAL, vendor: 0x1d6b, product: 0x0001, version: 1 },
            ff_effects_max: 0,
            absmax: [0; ABS_CNT],
            absmin: [0; ABS_CNT],
            absfuzz: [0; ABS_CNT],
            absflat: [0; ABS_CNT],
        };
        for (i, b) in name.bytes().take(UINPUT_MAX_NAME_SIZE - 1).enumerate() {
            dev.name[i] = b as libc::c_char;
        }
        for (axis, min, max) in abs {
            dev.absmin[*axis as usize] = *min;
            dev.absmax[*axis as usize] = *max;
        }

        // SAFETY: `dev` is a live, fully-initialised `UinputUserDev`; we hand the kernel
        // its exact byte length and do not retain the pointer.
        let n = unsafe {
            libc::write(
                fd,
                std::ptr::addr_of!(dev).cast(),
                std::mem::size_of::<UinputUserDev>(),
            )
        };
        if n != std::mem::size_of::<UinputUserDev>() as isize {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: UI_DEV_CREATE takes no argument.
        if unsafe { libc::ioctl(fd, UI_DEV_CREATE) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Device { file })
    }

    /// Queue one event. Nothing is delivered until [`Device::sync`].
    fn emit(&mut self, type_: u16, code: u16, value: i32) -> io::Result<()> {
        let ev = InputEventRaw {
            time: libc::timeval { tv_sec: 0, tv_usec: 0 },
            type_,
            code,
            value,
        };
        // SAFETY: `ev` is a live, fully-initialised `InputEventRaw` of exactly the length
        // passed; the kernel copies it and we do not retain the pointer.
        let n = unsafe {
            libc::write(
                self.file.as_raw_fd(),
                std::ptr::addr_of!(ev).cast(),
                std::mem::size_of::<InputEventRaw>(),
            )
        };
        if n != std::mem::size_of::<InputEventRaw>() as isize {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Deliver everything queued as one atomic report.
    fn sync(&mut self) -> io::Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // Best effort: the node also disappears when the fd closes, which is about to
        // happen anyway. Asking first means it is gone before any later open of the
        // same name, rather than lingering for as long as the kernel takes to notice.
        // SAFETY: UI_DEV_DESTROY takes no argument and the fd is still open here.
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY) };
    }
}

fn set_bit(fd: libc::c_int, req: libc::c_ulong, bit: i32) -> io::Result<()> {
    // SAFETY: every UI_SET_*BIT request takes an int by value.
    if unsafe { libc::ioctl(fd, req, bit) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ------------------------------------------------------------------ injector

/// How long to wait after creating the devices before trusting them.
///
/// `UI_DEV_CREATE` returns as soon as the kernel has the node; udev and the compositor
/// (or the greeter's X server) still have to notice it and start reading. Events written
/// in that window are delivered to nobody. Without this the first keystroke or two of a
/// freshly-started link vanish — which, when the thing being typed is a password, reads
/// as "it dropped a character" rather than "it was not listening yet".
const SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// Injection through two virtual kernel devices: a keyboard and an absolute pointer.
pub struct UinputInjector {
    kbd: Device,
    ptr: Device,
    /// Screen bounds the absolute axes were declared over.
    w: i32,
    h: i32,
    /// Where we have put the pointer. The kernel has no "where is the cursor" answer for
    /// a device that only writes, so this is the authoritative position *we* drive, and
    /// it is correct as long as nothing else is moving the pointer.
    x: i32,
    y: i32,
}

impl UinputInjector {
    /// Create the virtual devices for a screen of `w` x `h`.
    ///
    /// The size has to be given rather than detected: this backend exists for the case
    /// where there is no X server to ask, so it takes the configured geometry.
    pub fn new(w: i32, h: i32) -> Result<UinputInjector, InputError> {
        let w = w.max(1);
        let h = h.max(1);
        let keys = all_key_codes();
        let kbd = Device::create("mouse-rs virtual keyboard", &keys, &[], &[])
            .map_err(|e| Self::explain(e, "keyboard"))?;
        let ptr = Device::create(
            "mouse-rs virtual pointer",
            &[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
            &[REL_WHEEL, REL_HWHEEL],
            &[(ABS_X, 0, w - 1), (ABS_Y, 0, h - 1)],
        )
        .map_err(|e| Self::explain(e, "pointer"))?;

        std::thread::sleep(SETTLE);
        tracing::info!(w, h, keys = keys.len(), "uinput devices created");
        Ok(UinputInjector { kbd, ptr, w, h, x: w / 2, y: h / 2 })
    }

    /// Turn an open/ioctl failure into something that names the actual remedy.
    fn explain(e: io::Error, which: &str) -> InputError {
        if e.kind() == io::ErrorKind::PermissionDenied || e.kind() == io::ErrorKind::NotFound {
            InputError::Backend(format!(
                "cannot create the virtual {which}: {e}. /dev/uinput is root-only on a \
                 stock system — run this as root, or grant the device to a group with a \
                 udev rule. There is no unprivileged way to create a virtual input device."
            ))
        } else {
            InputError::Backend(format!("cannot create the virtual {which}: {e}"))
        }
    }

    fn code_for(key: &Key) -> Option<u16> {
        match key {
            Key::Named(n) => named_code(*n),
            Key::Char(c) => char_code(*c),
            // A raw X11 keysym has no keycode without the keymap that produced it.
            Key::Raw(_) => None,
        }
    }

    /// Emit the pointer's current position.
    fn flush_pos(&mut self) -> Result<(), InputError> {
        self.ptr.emit(EV_ABS, ABS_X, self.x).map_err(io_err)?;
        self.ptr.emit(EV_ABS, ABS_Y, self.y).map_err(io_err)?;
        self.ptr.sync().map_err(io_err)
    }
}

fn io_err(e: io::Error) -> InputError {
    InputError::Backend(format!("uinput write: {e}"))
}

fn button_code(b: Button) -> u16 {
    match b {
        Button::Left => BTN_LEFT,
        Button::Right => BTN_RIGHT,
        Button::Middle => BTN_MIDDLE,
    }
}

impl InputInjector for UinputInjector {
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.x = (self.x + dx).clamp(0, self.w - 1);
        self.y = (self.y + dy).clamp(0, self.h - 1);
        self.flush_pos()
    }

    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        self.x = x.clamp(0, self.w - 1);
        self.y = y.clamp(0, self.h - 1);
        self.flush_pos()
    }

    fn mouse_button(&mut self, button: Button, pressed: bool) -> Result<(), InputError> {
        self.ptr
            .emit(EV_KEY, button_code(button), i32::from(pressed))
            .map_err(io_err)?;
        self.ptr.sync().map_err(io_err)
    }

    fn mouse_scroll(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        if dy != 0 {
            self.ptr.emit(EV_REL, REL_WHEEL, dy).map_err(io_err)?;
        }
        if dx != 0 {
            self.ptr.emit(EV_REL, REL_HWHEEL, dx).map_err(io_err)?;
        }
        if dx != 0 || dy != 0 {
            self.ptr.sync().map_err(io_err)?;
        }
        Ok(())
    }

    fn key(&mut self, key: &Key, pressed: bool) -> Result<(), InputError> {
        // An unmappable key is dropped with a note rather than failing the whole
        // injection: one unknown key must not take the link down mid-sentence.
        let Some(code) = Self::code_for(key) else {
            // The key is not named: at the login screen it would be part of a password.
            tracing::debug!("no Linux keycode for a received key; dropped");
            return Ok(());
        };
        self.kbd.emit(EV_KEY, code, i32::from(pressed)).map_err(io_err)?;
        self.kbd.sync().map_err(io_err)
    }

    fn cursor_pos(&mut self) -> Result<(i32, i32), InputError> {
        // Our own notion, which is the only one available: a write-only device cannot be
        // asked. Accurate while we are the only thing driving the pointer.
        Ok((self.x, self.y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ioctl_encoding_matches_the_kernel_header() {
        // Checked against linux/uinput.h. These four are the whole reason a hand-written
        // binding either works or silently creates a device that emits nothing.
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_RELBIT, 0x4004_5566);
        assert_eq!(UI_SET_ABSBIT, 0x4004_5567);
    }

    #[test]
    fn the_abi_structs_are_the_sizes_the_kernel_expects() {
        // 80 name + 8 id + 4 ff + 4 arrays of 64 i32.
        assert_eq!(std::mem::size_of::<UinputUserDev>(), 80 + 8 + 4 + 4 * ABS_CNT * 4);
        // 16-byte timeval + u16 + u16 + i32 on a 64-bit target.
        assert_eq!(std::mem::size_of::<InputEventRaw>(), 24);
    }

    #[test]
    fn every_key_the_mapper_accepts_is_declared_on_the_device() {
        // The device can only emit codes registered before creation, so a key the
        // mapper resolves but the declaration missed would map fine and type nothing.
        let declared = all_key_codes();
        for c in "abcdefghijklmnopqrstuvwxyz0123456789-=[];'`\\,./ ".chars() {
            let code = char_code(c).unwrap_or_else(|| panic!("no code for {c:?}"));
            assert!(declared.contains(&code), "{c:?} -> {code} not declared");
        }
        for n in 1u8..=24 {
            let code = named_code(NamedKey::F(n)).unwrap_or_else(|| panic!("no code for F{n}"));
            assert!(declared.contains(&code), "F{n} -> {code} not declared");
        }
        for k in [
            NamedKey::Enter, NamedKey::ShiftL, NamedKey::CtrlR, NamedKey::AltGr,
            NamedKey::SuperL, NamedKey::Delete, NamedKey::PageDown, NamedKey::Menu,
        ] {
            let code = named_code(k).unwrap();
            assert!(declared.contains(&code), "{k:?} -> {code} not declared");
        }
    }

    #[test]
    fn the_function_key_block_is_not_contiguous() {
        // F1-F10 run together, F11/F12 sit apart, F13+ are elsewhere again. Treating
        // the range as one run is the obvious mistake and puts F11 on KEY_NUMLOCK.
        assert_eq!(named_code(NamedKey::F(1)), Some(59));
        assert_eq!(named_code(NamedKey::F(10)), Some(68));
        assert_eq!(named_code(NamedKey::F(11)), Some(87));
        assert_eq!(named_code(NamedKey::F(12)), Some(88));
        assert_eq!(named_code(NamedKey::F(13)), Some(183));
        assert_eq!(named_code(NamedKey::F(24)), Some(194));
        assert_eq!(named_code(NamedKey::F(25)), None, "there is no F25");
        assert_eq!(named_code(NamedKey::F(0)), None, "there is no F0");
    }

    #[test]
    fn altgr_is_the_right_alt_position() {
        assert_eq!(named_code(NamedKey::AltGr), named_code(NamedKey::AltR));
    }

    #[test]
    fn an_unmappable_key_is_declined_rather_than_guessed() {
        // A raw keysym cannot become a keycode without the keymap it came from, and a
        // wrong guess types the wrong character into a password field.
        assert_eq!(UinputInjector::code_for(&Key::Raw(0x1008_ff11)), None);
        assert_eq!(UinputInjector::code_for(&Key::Char('€')), None);
        assert_eq!(UinputInjector::code_for(&Key::Char('a')), Some(30));
    }

    #[test]
    fn a_shifted_character_resolves_to_its_physical_key() {
        // The wire carries the base character, but be defensive: 'A' and 'a' are the
        // same physical key and must not resolve differently.
        assert_eq!(char_code('A'), char_code('a'));
    }
}
