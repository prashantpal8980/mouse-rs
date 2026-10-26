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
    if std::env::var_os(OFF_SWITCH).is_some() {
        tracing::info!("input link NOT marked for Wi-Fi priority ({OFF_SWITCH} is set)");
        return;
    }
    match mark(stream) {
        Ok(how) => tracing::info!("input link marked for Wi-Fi priority ({how})"),
        Err(e) => tracing::warn!("could not mark the input link for Wi-Fi priority: {e}"),
    }
}

#[cfg(unix)]
fn mark(stream: &TcpStream) -> std::io::Result<&'static str> {
    let sock = socket2::SockRef::from(stream);
    if stream.local_addr()?.is_ipv4() {
        sock.set_tos(DSCP_EF_TOS)?;
    } else {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        sock.set_tclass_v6(DSCP_EF_TOS)?;
    }
    Ok("DSCP 46")
}

#[cfg(windows)]
fn mark(stream: &TcpStream) -> std::io::Result<&'static str> {
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
    // SAFETY: the socket is connected (so no destination address is needed) and
    // stays open for the call; `flow` is a valid out-pointer.
    let ok = unsafe {
        QOSAddSocketToFlow(
            handle as _,
            stream.as_raw_socket() as _,
            std::ptr::null(),
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
