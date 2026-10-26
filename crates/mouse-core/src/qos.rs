//! Wi-Fi priority for the input connection.
//!
//! On Wi-Fi the router keeps one queue per device. When a video loads on the same
//! machine, megabytes land in that queue at once and the pointer's tiny frames wait
//! behind them: the owner measured 0.3–1 s freezes on 26 Oct 2026 while a video
//! buffered. Wi-Fi has four priority queues (WMM: voice, video, best effort,
//! background), chosen per packet from its DSCP mark, and every router since
//! 802.11n honours them. Marking the input connection as voice puts mouse and
//! keyboard frames ahead of the video. They are a few kilobytes a second, so the
//! video doesn't measurably slow down. The bulk (file) lane is left unmarked on
//! purpose: a big copy must not jump the queue.
//!
//! Neither platform needs admin rights for this. Linux takes the DSCP byte directly;
//! Windows ignores `IP_TOS` from applications and wants its QoS API (qWAVE), the one
//! call-and-meeting apps use. Failure is logged and harmless: the link just runs
//! unprioritised, as it did before.

use tokio::net::TcpStream;

/// DSCP 46, "expedited forwarding": the mark for interactive, low-latency traffic.
/// Linux's Wi-Fi stack maps it to the voice (or, on older kernels, video) queue.
#[cfg(unix)]
const DSCP_EF_TOS: u32 = 46 << 2;

/// Set (to anything) to leave the input link unmarked, for a with/without comparison.
const OFF_SWITCH: &str = "MOUSE_RS_NO_WIFI_PRIORITY";

/// Mark an input connection for Wi-Fi priority. Call once it is connected.
pub fn prioritise_input(stream: &TcpStream) {
    prioritise("input link", stream, None);
    wifi_low_latency();
}

/// Windows: put every Wi-Fi adapter in "media streaming mode" with background scanning off,
/// for as long as mouse-rs runs. This is what game-streaming apps do.
///
/// On 26 Oct 2026, on both the home router and a phone hotspot, every minute on Wi-Fi had a
/// pointer freeze of 130–150 ms that the cable never had: the laptop's adapter leaving the
/// channel to scan for other networks, or dozing to save power. Video buffers through that;
/// a pointer can't. Windows undoes both settings by itself when our WLAN handle closes, which
/// happens when the process exits, so nothing outlives a quit (R33). Safe to call often: each
/// adapter is set once, and a new adapter is picked up on the next connection.
#[cfg(windows)]
pub fn wifi_low_latency() {
    use std::ptr::{null, null_mut};
    use std::sync::Mutex;
    use windows_sys::Win32::NetworkManagement::WiFi::{
        wlan_intf_opcode_background_scan_enabled, wlan_intf_opcode_media_streaming_mode,
        WlanEnumInterfaces, WlanFreeMemory, WlanOpenHandle, WlanSetInterface, WLAN_INTERFACE_INFO,
        WLAN_INTERFACE_INFO_LIST,
    };

    /// The WLAN handle (as an integer, to be Send) and the adapters already set on it.
    static STATE: Mutex<Option<(usize, Vec<u128>)>> = Mutex::new(None);

    if std::env::var_os(OFF_SWITCH).is_some() {
        return;
    }
    let Ok(mut state) = STATE.lock() else { return };
    if state.is_none() {
        let mut version = 0u32;
        let mut handle = null_mut();
        // SAFETY: plain out-parameters; the handle is kept for the life of the process.
        let rc = unsafe { WlanOpenHandle(2, null(), &mut version, &mut handle) };
        if rc != 0 {
            // No WLAN service: a machine without Wi-Fi. Nothing to do, and nothing to retry.
            tracing::info!("no Wi-Fi service (error {rc}); Wi-Fi low-latency mode not needed");
            *state = Some((0, Vec::new()));
            return;
        }
        *state = Some((handle as usize, Vec::new()));
    }
    let Some((handle, done)) = state.as_mut() else { return };
    if *handle == 0 {
        return;
    }
    let handle = *handle as windows_sys::Win32::Foundation::HANDLE;
    let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
    // SAFETY: the handle is open; the list is freed below with WlanFreeMemory.
    if unsafe { WlanEnumInterfaces(handle, null(), &mut list) } != 0 || list.is_null() {
        return;
    }
    // SAFETY: the list holds dwNumberOfItems entries laid out from InterfaceInfo.
    let infos: &[WLAN_INTERFACE_INFO] = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!((*list).InterfaceInfo).cast::<WLAN_INTERFACE_INFO>(),
            (*list).dwNumberOfItems as usize,
        )
    };
    for info in infos {
        let g = info.InterfaceGuid;
        let id = (u128::from(g.data1) << 96)
            | (u128::from(g.data2) << 80)
            | (u128::from(g.data3) << 64)
            | u128::from(u64::from_be_bytes(g.data4));
        if done.contains(&id) {
            continue;
        }
        let on: u32 = 1;
        let off: u32 = 0;
        // SAFETY: each call passes a 4-byte BOOL, as these opcodes take.
        let streaming = unsafe {
            WlanSetInterface(handle, &g, wlan_intf_opcode_media_streaming_mode, 4, (&on as *const u32).cast(), null())
        };
        let no_scan = unsafe {
            WlanSetInterface(handle, &g, wlan_intf_opcode_background_scan_enabled, 4, (&off as *const u32).cast(), null())
        };
        let len = info.strInterfaceDescription.iter().position(|&c| c == 0).unwrap_or(256);
        let name = String::from_utf16_lossy(&info.strInterfaceDescription[..len]);
        let said = |rc: u32| if rc == 0 { "on".to_string() } else { format!("refused (error {rc})") };
        tracing::info!(
            "Wi-Fi low-latency mode for {name}: media streaming {}, background scans off {}",
            said(streaming),
            said(no_scan)
        );
        done.push(id);
    }
    // SAFETY: allocated by WlanEnumInterfaces above.
    unsafe { WlanFreeMemory(list.cast()) };
}

/// Nothing to do outside Windows: Linux needs root for the same, and Kali's power saving is
/// set by hand (`iw dev wlan0 set power_save off`).
#[cfg(not(windows))]
pub fn wifi_low_latency() {}

/// Mark the UDP motion socket for Wi-Fi priority, for datagrams to `to`.
pub(crate) fn prioritise_datagrams(sock: &tokio::net::UdpSocket, to: std::net::SocketAddr) {
    prioritise("pointer motion", sock, Some(to));
}

#[cfg(unix)]
trait Sock: std::os::fd::AsFd {}
#[cfg(unix)]
impl<T: std::os::fd::AsFd> Sock for T {}
#[cfg(windows)]
trait Sock: std::os::windows::io::AsSocket {}
#[cfg(windows)]
impl<T: std::os::windows::io::AsSocket> Sock for T {}

fn prioritise(what: &str, sock: &impl Sock, to: Option<std::net::SocketAddr>) {
    if std::env::var_os(OFF_SWITCH).is_some() {
        tracing::info!("{what} NOT marked for Wi-Fi priority ({OFF_SWITCH} is set)");
        return;
    }
    match mark(sock, to) {
        Ok(how) => tracing::info!("{what} marked for Wi-Fi priority ({how})"),
        Err(e) => tracing::warn!("could not mark {what} for Wi-Fi priority: {e}"),
    }
}

#[cfg(unix)]
fn mark(sock: &impl Sock, _to: Option<std::net::SocketAddr>) -> std::io::Result<&'static str> {
    let sock = socket2::SockRef::from(sock);
    let v4 = sock.local_addr()?.as_socket().map_or(true, |a| a.is_ipv4());
    if v4 {
        sock.set_tos(DSCP_EF_TOS)?;
    } else {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        sock.set_tclass_v6(DSCP_EF_TOS)?;
    }
    Ok("DSCP 46")
}

#[cfg(windows)]
fn mark(sock: &impl Sock, to: Option<std::net::SocketAddr>) -> std::io::Result<&'static str> {
    use std::os::windows::io::AsRawSocket;
    use std::sync::OnceLock;
    use windows_sys::Win32::NetworkManagement::QoS::{
        QOSAddSocketToFlow, QOSCreateHandle, QOSTrafficTypeVoice, QOS_NON_ADAPTIVE_FLOW,
        QOS_VERSION,
    };

    // One handle for the life of the process. Flows on it end when their socket
    // closes, and the handle itself goes with the process.
    static HANDLE: OnceLock<Option<usize>> = OnceLock::new();
    let handle = HANDLE.get_or_init(|| {
        let version = QOS_VERSION { MajorVersion: 1, MinorVersion: 0 };
        let mut h = std::ptr::null_mut();
        // SAFETY: both pointers are valid for the call.
        let ok = unsafe { QOSCreateHandle(&version, &mut h) };
        (ok != 0).then_some(h as usize)
    });
    let Some(handle) = *handle else {
        return Err(std::io::Error::other("QOSCreateHandle failed"));
    };
    let mut flow = 0u32;
    // A connected socket needs no destination; an unconnected one (the server's UDP
    // socket) names the peer the flow is for.
    let dest = to.map(socket2::SockAddr::from);
    // SAFETY: the socket stays open for the call, `dest` outlives it, and `flow` is a
    // valid out-pointer.
    let ok = unsafe {
        QOSAddSocketToFlow(
            handle as _,
            sock.as_socket().as_raw_socket() as _,
            dest.as_ref().map_or(std::ptr::null(), |d| d.as_ptr() as *const _),
            QOSTrafficTypeVoice,
            QOS_NON_ADAPTIVE_FLOW,
            &mut flow,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok("qWAVE voice")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_input_socket_carries_the_priority_mark() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, _) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let client = client.unwrap();
        prioritise_input(&client);
        assert_eq!(socket2::SockRef::from(&client).tos().unwrap(), DSCP_EF_TOS);
    }
}
