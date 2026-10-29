//! The update notice (R36): "Version x.y is available — Update".
//!
//! A release publishes `latest.json` and `latest.json.sig` next to its files. The JSON
//! names the version and, per platform, the binary's URL, size and SHA-256. The `.sig`
//! is an Ed25519 signature over the JSON's exact bytes, made with a key that never
//! leaves the release machine (`~/.config/mouse-rs-release/update-signing.pem`); the
//! public half is built in below. So a download is trusted only if the signed JSON says
//! so, whatever the server or network in between did.
//!
//! The check runs at start and once a day, on a thread of this process only: nothing is
//! left running after Quit (R33). **Update** downloads next to the running binary,
//! checks size and hash, then swaps the file; the user restarts when they choose.
//!
//! Test builds point somewhere else and claim another version through two build-time
//! variables, so the whole path can be tried against a local fake release:
//! `MOUSE_RS_UPDATE_URL` and `MOUSE_RS_VERSION` (see `scripts/fake-release.sh`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// This build's version as users see it.
pub const VERSION: &str = match option_env!("MOUSE_RS_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Where the signed release description lives.
pub const MANIFEST_URL: &str = match option_env!("MOUSE_RS_UPDATE_URL") {
    Some(u) => u,
    None => "https://github.com/prashantpal8980/mouse-rs-downloads/releases/latest/download/latest.json",
};

/// The page a user can download from by hand, when updating in place is not possible.
pub const DOWNLOAD_PAGE: &str = "https://github.com/prashantpal8980/mouse-rs-downloads/releases/latest";

/// Ed25519 public key matching the release signing key.
const PUBLIC_KEY: [u8; 32] = [
    0x97, 0x9a, 0x5e, 0xe7, 0xd2, 0xed, 0x21, 0x80, 0x71, 0xd2, 0xca, 0xa0, 0xaa, 0x8d, 0x2c, 0xaf, 0x77, 0xcf, 0x29,
    0x9e, 0x39, 0xe4, 0x82, 0x6f, 0x7d, 0x78, 0xa1, 0x8d, 0x61, 0x61, 0x5d, 0x21,
];

/// The key in `latest.json`'s `files` for this build.
pub const PLATFORM: &str = if cfg!(windows) { "windows-x86_64" } else { "linux-x86_64" };

/// Far more than a real `latest.json`; stops a hostile server from feeding us forever.
const MANIFEST_MAX: u64 = 64 * 1024;
/// Far more than the binary; same reason.
const BINARY_MAX: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// One or two sentences for the banner.
    #[serde(default)]
    pub notes: String,
    pub files: std::collections::HashMap<String, Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Default)]
pub enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    /// The last check did not get an answer. Kept quiet: no banner for a network blip.
    CheckFailed(String),
    Available(Manifest),
    Downloading { version: String, done: u64, total: u64 },
    /// The new file is in place; it runs from the next start.
    Ready(String),
    /// The update did not install; the reason is for the user.
    Failed(String),
}

pub type Shared = Arc<Mutex<State>>;

fn set(shared: &Shared, s: State) {
    *shared.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

pub fn get(shared: &Shared) -> State {
    shared.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The running binary's path, read once at start. After the swap, Linux reports the
/// old file as "… (deleted)", so it cannot be asked for again.
pub fn exe_path() -> Option<&'static Path> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok()).as_deref()
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .user_agent(&format!("mouse-rs/{VERSION}"))
        .build()
}

fn fetch_small(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let resp = agent.get(url).call().map_err(|e| short_error(&e))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MANIFEST_MAX + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() as u64 > MANIFEST_MAX {
        return Err("the release description is too large".into());
    }
    Ok(buf)
}

fn short_error(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("the server answered {code}"),
        ureq::Error::Transport(t) => t.to_string(),
    }
}

/// Checks the signature and reads the JSON. Nothing in an unsigned file is believed.
pub fn verify(json: &[u8], sig_hex: &[u8]) -> Result<Manifest, String> {
    let sig = decode_hex(std::str::from_utf8(sig_hex).map_err(|_| "bad signature file")?.trim())
        .ok_or("bad signature file")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, PUBLIC_KEY)
        .verify(json, &sig)
        .map_err(|_| "the release description is not signed by mouse-rs".to_string())?;
    serde_json::from_slice(json).map_err(|e| format!("unreadable release description: {e}"))
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// `1.0.10` > `1.0.9`; a release > its own pre-release (`1.0.0` > `1.0.0-test2`);
/// pre-releases compare part by part, numbers as numbers (`test10` > `test9`).
pub fn is_newer(candidate: &str, current: &str) -> bool {
    version_key(candidate) > version_key(current)
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Part {
    Num(u64),
    Text(String),
}

fn split_parts(s: &str) -> Vec<Part> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut digits = false;
    for ch in s.chars() {
        if ch == '.' || ch == '-' {
            if !cur.is_empty() {
                out.push(part(&cur, digits));
                cur.clear();
            }
            continue;
        }
        if !cur.is_empty() && ch.is_ascii_digit() != digits {
            out.push(part(&cur, digits));
            cur.clear();
        }
        digits = ch.is_ascii_digit();
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(part(&cur, digits));
    }
    out
}

fn part(s: &str, digits: bool) -> Part {
    if digits {
        Part::Num(s.parse().unwrap_or(u64::MAX))
    } else {
        Part::Text(s.to_ascii_lowercase())
    }
}

/// (release numbers, is-a-release, pre-release parts): a release sorts after any of its
/// pre-releases because `true > false`.
fn version_key(v: &str) -> (Vec<u64>, bool, Vec<Part>) {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let (main, pre) = match v.split_once('-') {
        Some((m, p)) => (m, Some(p)),
        None => (v, None),
    };
    let mut nums: Vec<u64> = main.split('.').map(|n| n.parse().unwrap_or(0)).collect();
    while nums.len() < 3 {
        nums.push(0);
    }
    (nums, pre.is_none(), pre.map(split_parts).unwrap_or_default())
}

/// Asks the release server whether there is something newer. Runs on its own thread.
pub fn spawn_check(shared: Shared) {
    set(&shared, State::Checking);
    std::thread::spawn(move || {
        let result = check();
        let next = match result {
            Ok(Some(m)) => {
                tracing::info!("update: version {} is available (this is {VERSION})", m.version);
                State::Available(m)
            }
            Ok(None) => {
                tracing::info!("update: {VERSION} is the newest");
                State::UpToDate
            }
            Err(e) => {
                tracing::info!("update: check did not get an answer: {e}");
                State::CheckFailed(e)
            }
        };
        // A check that started before an install must not overwrite the install's state.
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*s, State::Checking) {
            *s = next;
        }
    });
}

fn check() -> Result<Option<Manifest>, String> {
    let agent = agent();
    let json = fetch_small(&agent, MANIFEST_URL)?;
    let sig = fetch_small(&agent, &format!("{MANIFEST_URL}.sig"))?;
    let m = verify(&json, &sig)?;
    if !is_newer(&m.version, VERSION) {
        return Ok(None);
    }
    if !m.files.contains_key(PLATFORM) {
        // A release without a build for this system is not an update for it.
        tracing::info!("update: {} has no {PLATFORM} build", m.version);
        return Ok(None);
    }
    Ok(Some(m))
}

/// Downloads, checks and swaps in the new binary. Runs on its own thread.
pub fn spawn_install(shared: Shared, m: Manifest) {
    let Some(asset) = m.files.get(PLATFORM).cloned() else {
        set(&shared, State::Failed("this release has no build for this system".into()));
        return;
    };
    set(&shared, State::Downloading { version: m.version.clone(), done: 0, total: asset.size });
    std::thread::spawn(move || match install(&shared, &m.version, &asset) {
        Ok(()) => {
            tracing::info!("update: version {} installed; it runs from the next start", m.version);
            set(&shared, State::Ready(m.version));
        }
        Err(e) => {
            tracing::warn!("update: version {} did not install: {e}", m.version);
            set(&shared, State::Failed(e));
        }
    });
}

fn install(shared: &Shared, version: &str, asset: &Asset) -> Result<(), String> {
    let exe = exe_path().ok_or("could not find where mouse-rs is installed")?;
    install_to(shared, version, asset, exe)
}

fn install_to(shared: &Shared, version: &str, asset: &Asset, exe: &Path) -> Result<(), String> {
    if asset.size > BINARY_MAX {
        return Err("the new version is unexpectedly large".into());
    }
    let new = sibling(exe, "new");
    let result = download(shared, version, asset, &new).and_then(|()| swap(exe, &new));
    if result.is_err() {
        let _ = std::fs::remove_file(&new);
    }
    result
}

/// `mouse-rs` → `mouse-rs.new`, `mouse-rs.exe` → `mouse-rs.exe.new`.
fn sibling(exe: &Path, ext: &str) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

fn download(shared: &Shared, version: &str, asset: &Asset, to: &Path) -> Result<(), String> {
    let mut file = std::fs::File::create(to).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("mouse-rs cannot write to {}", to.parent().map(|d| d.display().to_string()).unwrap_or_default())
        } else {
            format!("could not save the download: {e}")
        }
    })?;
    let resp = agent().get(&asset.url).call().map_err(|e| short_error(&e))?;
    let mut reader = resp.into_reader().take(asset.size + 1);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("the download stopped: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        std::io::Write::write_all(&mut file, &buf[..n]).map_err(|e| format!("could not save the download: {e}"))?;
        done += n as u64;
        set(shared, State::Downloading { version: version.into(), done, total: asset.size });
    }
    std::io::Write::flush(&mut file).map_err(|e| e.to_string())?;
    drop(file);
    if done != asset.size {
        return Err(format!("the download has {done} bytes, the release says {}", asset.size));
    }
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if !got.eq_ignore_ascii_case(asset.sha256.trim()) {
        return Err("the download does not match the signed release".into());
    }
    Ok(())
}

#[cfg(unix)]
fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    // Atomic: this process keeps running from the old file, the next start gets the new.
    std::fs::rename(new, exe).map_err(|e| format!("could not replace {}: {e}", exe.display()))
}

#[cfg(windows)]
fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    // A running .exe cannot be overwritten, but it can be renamed out of the way.
    let old = sibling(exe, "old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("could not move the running copy aside: {e}"))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("could not put the new version in place: {e}"));
    }
    Ok(())
}

/// Starts the (new) binary with this one's arguments and tells it to wait for this
/// process to be gone, so the two never hold the port or the input hooks at once.
pub fn relaunch() -> Result<(), String> {
    let exe = exe_path().ok_or("could not find where mouse-rs is installed")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1)).env(WAIT_PID_VAR, std::process::id().to_string());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window for the new copy (the binary is a console app).
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("could not start the new version: {e}"))
}

const WAIT_PID_VAR: &str = "MOUSE_RS_WAIT_PID";

/// Called first thing in `main`: after an update restart, wait (a few seconds at most)
/// for the copy that started us to exit, then tidy what the swap left behind.
pub fn after_restart() {
    let Some(pid) = std::env::var(WAIT_PID_VAR).ok().and_then(|p| p.parse::<u32>().ok()) else {
        return;
    };
    std::env::remove_var(WAIT_PID_VAR);
    wait_for_exit(pid, Duration::from_secs(5));
    #[cfg(windows)]
    if let Some(exe) = exe_path() {
        let _ = std::fs::remove_file(sibling(exe, "old"));
    }
}

#[cfg(target_os = "linux")]
fn wait_for_exit(pid: u32, limit: Duration) {
    let t = std::time::Instant::now();
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    while proc_dir.exists() && t.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(all(windows, feature = "gui"))]
fn wait_for_exit(pid: u32, limit: Duration) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // SAFETY: plain Win32 calls on a handle we open and close here; a null handle (the
    // process is already gone) is checked before use.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, limit.as_millis() as u32);
            CloseHandle(h);
        }
    }
}

#[cfg(not(any(all(windows, feature = "gui"), target_os = "linux")))]
fn wait_for_exit(_pid: u32, limit: Duration) {
    std::thread::sleep(limit.min(Duration::from_secs(2)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_the_way_people_read_them() {
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(is_newer("1.0.10", "1.0.9"));
        assert!(is_newer("1.1", "1.0.9"));
        assert!(is_newer("1.0.0", "1.0.0-test2"));
        assert!(is_newer("1.0.0-test2", "1.0.0-test1"));
        assert!(is_newer("1.0.0-test10", "1.0.0-test9"));
        assert!(is_newer("v2.0.0", "1.9.9"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0-test1", "1.0.0"));
        assert!(!is_newer("0.9.9", "1.0.0"));
    }

    /// Signed with the real release key by `openssl pkeyutl -sign -rawin`, the way
    /// `scripts/sign-release.sh` signs, so this also proves the script and the app agree.
    #[test]
    fn a_description_signed_with_the_release_key_is_accepted() {
        let json = br#"{"version":"9.9.9","notes":"test vector","files":{}}"#;
        let sig = b"a7cc9cd690279b536b88a8f975e47a426f93f68fba5bd3c85ca80b696960142d88d582215bf00da441a4bd8263aa84074288cafc078c32f525187ad15e8f9507\n";
        let m = verify(json, sig).expect("signed by the release key");
        assert_eq!(m.version, "9.9.9");
        let mut altered = json.to_vec();
        altered[13] = b'8';
        assert!(verify(&altered, sig).is_err());
    }

    #[test]
    fn an_unsigned_or_altered_description_is_refused() {
        let json = br#"{"version":"9.9.9","files":{}}"#;
        assert!(verify(json, b"00").is_err());
        assert!(verify(json, &[b'a'; 128]).is_err());
        assert!(verify(json, b"not hex at all").is_err());
    }

    /// Serves `body` once per connection on a local port, the way a release host would.
    fn serve(body: Vec<u8>, connections: usize) -> String {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in l.incoming().take(connections) {
                let mut s = stream.unwrap();
                let mut req = [0u8; 2048];
                let _ = s.read(&mut req);
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(&body);
            }
        });
        format!("http://{addr}/mouse-rs")
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mouse-rs-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_good_download_replaces_the_binary() {
        let new = b"the new version".to_vec();
        let sha: String = Sha256::digest(&new).iter().map(|b| format!("{b:02x}")).collect();
        let asset = Asset { url: serve(new.clone(), 1), size: new.len() as u64, sha256: sha };
        let dir = scratch("good");
        let exe = dir.join(if cfg!(windows) { "mouse-rs.exe" } else { "mouse-rs" });
        std::fs::write(&exe, b"the old version").unwrap();
        let shared = Shared::default();
        install_to(&shared, "9.9.9", &asset, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), new);
        assert!(!sibling(&exe, "new").exists(), "nothing left behind");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_that_does_not_match_leaves_the_binary_alone() {
        let asset = Asset {
            url: serve(b"something else".to_vec(), 2),
            size: 14,
            sha256: "00".repeat(32),
        };
        let dir = scratch("bad");
        let exe = dir.join("mouse-rs");
        std::fs::write(&exe, b"the old version").unwrap();
        let shared = Shared::default();
        let e = install_to(&shared, "9.9.9", &asset, &exe).unwrap_err();
        assert!(e.contains("does not match"), "{e}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"the old version");
        assert!(!sibling(&exe, "new").exists(), "the bad download is removed");
        // A short download is refused too.
        let short = Asset { size: 99, ..asset };
        assert!(install_to(&shared, "9.9.9", &short, &exe).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"the old version");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_new_file_sits_next_to_the_running_one() {
        assert_eq!(sibling(Path::new("/opt/m/mouse-rs"), "new"), PathBuf::from("/opt/m/mouse-rs.new"));
        assert_eq!(sibling(Path::new("C:/m/mouse-rs.exe"), "old"), PathBuf::from("C:/m/mouse-rs.exe.old"));
    }
}
