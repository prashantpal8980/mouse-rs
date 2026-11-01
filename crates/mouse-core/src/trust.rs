//! The computers this one is paired with, as the link sees them right now (R44).
//!
//! The config file holds the list, but the link needs it live: a computer paired a moment
//! ago must be able to connect at once, before the panel has saved anything, and one the
//! user removed must be refused at once, before sharing is restarted. So the running
//! engine reads the list from here, and the panel writes changes both here and to the
//! file.
//!
//! The store is keyed by this computer's own device ID. In the app there is only ever one,
//! but the tests run both ends of a link in one process, and each end must see only its
//! own list.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::PairedDevice;

#[derive(Default)]
struct Store {
    /// Own device ID → the computers it is paired with.
    paired: HashMap<String, Vec<PairedDevice>>,
    /// Own device ID → pairings made by the link that the panel has not saved yet.
    unsaved: HashMap<String, Vec<PairedDevice>>,
    /// Own device ID → paired computers that proved themselves under a new name (B61),
    /// device ID → name, for the panel to save.
    renamed: HashMap<String, HashMap<String, String>>,
    /// Own device ID → the device holding this server's input link, if any.
    input_peer: HashMap<String, String>,
    /// Own device ID → the computer this one is to be with (R45): the one it was last
    /// with, or the one the user clicked. A client connects to this one only.
    choice: HashMap<String, String>,
    /// Own device ID → on a server, the computer the user clicked Connect on, which is the
    /// only one let in until it arrives (R45).
    reserved: HashMap<String, String>,
    /// Own device ID → a computer this one has just connected with, for the panel to
    /// remember as the last one.
    last: HashMap<String, String>,
    /// Own device ID → on a server, every computer connected right now (R46).
    connected: HashMap<String, Vec<String>>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Store) -> T) -> T {
    let mut g = STORE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Store::default))
}

fn put(list: &mut Vec<PairedDevice>, dev: PairedDevice) {
    list.retain(|d| d.id != dev.id);
    list.push(dev);
}

/// Start from the saved list. Pairings the link made that the panel has not saved yet
/// are kept, so restarting sharing at the wrong moment cannot lose one.
pub fn load(own: &str, saved: &[PairedDevice]) {
    with(|s| {
        let mut list = saved.to_vec();
        for d in s.unsaved.get(own).into_iter().flatten() {
            put(&mut list, d.clone());
        }
        s.paired.insert(own.to_owned(), list);
    })
}

/// The pairing with `peer`, if this computer has one.
pub fn get(own: &str, peer: &str) -> Option<PairedDevice> {
    with(|s| s.paired.get(own)?.iter().find(|d| d.id == peer).cloned())
}

/// Every computer this one is paired with.
pub fn all(own: &str) -> Vec<PairedDevice> {
    with(|s| s.paired.get(own).cloned().unwrap_or_default())
}

/// A new pairing (or a fresh one with a computer paired before, which replaces it). It is
/// live at once, and waits in [`take_unsaved`] for the panel to save.
pub fn add(own: &str, dev: PairedDevice) {
    with(|s| {
        put(s.paired.entry(own.to_owned()).or_default(), dev.clone());
        put(s.unsaved.entry(own.to_owned()).or_default(), dev);
    })
}

/// Pairings made since this was last asked, for the panel to save.
pub fn take_unsaved(own: &str) -> Vec<PairedDevice> {
    with(|s| s.unsaved.remove(own).unwrap_or_default())
}

/// A paired computer has just proved itself, calling itself `name`. If it was renamed
/// since pairing, use the new name from now on and keep it for the panel to save (B61).
/// Only ever called after its proof checked out, so the name is the computer's own.
pub fn note_name(own: &str, peer: &str, name: &str) {
    let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    with(|s| {
        let Some(d) = s.paired.get_mut(own).and_then(|l| l.iter_mut().find(|d| d.id == peer)) else {
            return;
        };
        if d.name == name {
            return;
        }
        d.name = name.to_owned();
        s.renamed.entry(own.to_owned()).or_default().insert(peer.to_owned(), name.to_owned());
    })
}

/// Names learned by [`note_name`] since this was last asked: (device ID, name).
pub fn take_renamed(own: &str) -> Vec<(String, String)> {
    with(|s| s.renamed.remove(own).map(|m| m.into_iter().collect()).unwrap_or_default())
}

/// Forget `peer`: from now on it is refused.
pub fn remove(own: &str, peer: &str) {
    with(|s| {
        if let Some(l) = s.paired.get_mut(own) {
            l.retain(|d| d.id != peer);
        }
        if let Some(l) = s.unsaved.get_mut(own) {
            l.retain(|d| d.id != peer);
        }
    })
}

/// Which device holds this server's input link, so the bulk lane can be refused to any
/// other paired computer.
pub fn set_input_peer(own: &str, peer: Option<&str>) {
    with(|s| match peer {
        Some(p) => {
            s.input_peer.insert(own.to_owned(), p.to_owned());
        }
        None => {
            s.input_peer.remove(own);
        }
    })
}

pub fn input_peer(own: &str) -> Option<String> {
    with(|s| s.input_peer.get(own).cloned())
}

/// Clear the input-link holder, but only if it is still `peer`: a newer session from
/// someone else must not be wiped by an older one ending.
pub fn clear_input_peer(own: &str, peer: &str) {
    with(|s| {
        if s.input_peer.get(own).map(String::as_str) == Some(peer) {
            s.input_peer.remove(own);
        }
    })
}

fn set(map: &mut HashMap<String, String>, own: &str, v: Option<&str>) {
    match v {
        Some(v) if !v.is_empty() => {
            map.insert(own.to_owned(), v.to_owned());
        }
        _ => {
            map.remove(own);
        }
    }
}

/// The computer this one is to be with (R45). None: not chosen yet, so any paired one.
pub fn set_choice(own: &str, peer: Option<&str>) {
    with(|s| set(&mut s.choice, own, peer))
}

pub fn choice(own: &str) -> Option<String> {
    with(|s| s.choice.get(own).cloned())
}

/// Server: let only `peer` in until it arrives (the user clicked Connect on it).
pub fn reserve(own: &str, peer: Option<&str>) {
    with(|s| set(&mut s.reserved, own, peer))
}

pub fn reserved(own: &str) -> Option<String> {
    with(|s| s.reserved.get(own).cloned())
}

/// This computer just connected with `peer`: it becomes the one to be with, and waits in
/// [`take_last`] for the panel to remember.
pub fn connected_with(own: &str, peer: &str) {
    with(|s| {
        s.choice.insert(own.to_owned(), peer.to_owned());
        s.last.insert(own.to_owned(), peer.to_owned());
        if s.reserved.get(own).map(String::as_str) == Some(peer) {
            s.reserved.remove(own);
        }
    })
}

/// Server: the computers connected right now (R46).
pub fn set_connected(own: &str, peers: &[String]) {
    with(|s| {
        s.connected.insert(own.to_owned(), peers.to_vec());
    })
}

pub fn connected(own: &str) -> Vec<String> {
    with(|s| s.connected.get(own).cloned().unwrap_or_default())
}

/// The computer connected with since this was last asked, if any.
pub fn take_last(own: &str) -> Option<String> {
    with(|s| s.last.remove(own))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, key: &str) -> PairedDevice {
        PairedDevice { id: id.into(), name: id.into(), key: key.into(), paired_at: 0 }
    }

    #[test]
    fn each_computer_sees_only_its_own_pairings() {
        load("t-own-a", &[dev("t-peer", "k1")]);
        load("t-own-b", &[]);
        assert_eq!(get("t-own-a", "t-peer").unwrap().key, "k1");
        assert!(get("t-own-b", "t-peer").is_none());
    }

    #[test]
    fn a_new_pairing_is_live_at_once_and_survives_a_reload_until_saved() {
        load("t-own-c", &[]);
        add("t-own-c", dev("t-new", "k2"));
        assert!(get("t-own-c", "t-new").is_some());
        // Sharing restarted before the panel saved: still there.
        load("t-own-c", &[]);
        assert!(get("t-own-c", "t-new").is_some());
        assert_eq!(take_unsaved("t-own-c").len(), 1);
        assert!(take_unsaved("t-own-c").is_empty());
    }

    #[test]
    fn connecting_makes_that_computer_the_one_to_be_with() {
        set_choice("t-own-e", Some("a"));
        reserve("t-own-e", Some("b"));
        connected_with("t-own-e", "b");
        assert_eq!(choice("t-own-e").as_deref(), Some("b"));
        assert_eq!(reserved("t-own-e"), None, "the one waited for arrived");
        assert_eq!(take_last("t-own-e").as_deref(), Some("b"));
        assert_eq!(take_last("t-own-e"), None);
        set_input_peer("t-own-e", Some("b"));
        clear_input_peer("t-own-e", "a");
        assert_eq!(input_peer("t-own-e").as_deref(), Some("b"), "only the holder clears itself");
        clear_input_peer("t-own-e", "b");
        assert_eq!(input_peer("t-own-e"), None);
    }

    #[test]
    fn pairing_again_replaces_and_removing_refuses() {
        load("t-own-d", &[dev("t-p", "old")]);
        add("t-own-d", dev("t-p", "new"));
        assert_eq!(all("t-own-d").len(), 1);
        assert_eq!(get("t-own-d", "t-p").unwrap().key, "new");
        remove("t-own-d", "t-p");
        assert!(get("t-own-d", "t-p").is_none());
        assert!(take_unsaved("t-own-d").is_empty(), "a removed pairing is not saved later");
    }
}
