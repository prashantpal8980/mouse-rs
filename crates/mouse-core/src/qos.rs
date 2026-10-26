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
}

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
