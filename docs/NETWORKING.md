# Networking: finding the peer and following the network

mouse-rs works when both machines share **any** network: a direct Ethernet cable, a
home router, Wi-Fi, or a phone hotspot. It does not depend on one fixed address, and it
moves between networks without being restarted.

## Ports

| Port | Protocol | Used for |
|---|---|---|
| 24800 | TCP | Input link (keyboard, mouse, clipboard text) and the bulk lane for files |
| 24801 | UDP | Discovery beacons (`discovery_port` in the config) |

On Windows, allow mouse-rs through the firewall for **each** network profile it will be
used on (Private, and Public if you use a hotspot).

## How the client finds the server

1. **Beacons.** Every 2 s the server broadcasts a small UDP beacon on each network it is
   attached to. The beacon lists its current addresses. The beacon is signed with an
   HMAC-SHA256 keyed by the shared secret, so a machine without the secret can't redirect
   the client. A beacon only *offers* addresses; the real TLS + HMAC handshake still
   authenticates the server.
2. **Candidates.** On each attempt the client builds a list, in order:
   the address that last worked, then the configured `server_host`, then every address heard
   in a beacon in the last 8 s.
3. **Race.** The client connects to all candidates at once and keeps the first that
   answers within 3 s. The fastest working path wins.

So a wrong or stale `server_host` costs nothing as long as both machines can hear each
other's beacons.

## Following network changes

Both machines poll their network interfaces every 2 s. When the set of addresses changes
(cable unplugged or plugged in, Wi-Fi joined, hotspot switched), the client:

- drops the current session with reason `NetworkChanged`;
- forgets the last good address;
- reconnects at once, racing the new candidates.

On the server, the new authenticated connection **supersedes** the old one immediately
(`peer disconnected: Superseded`), instead of waiting for the stale one to time out.

Measured on 13 October 2026, unplugging and re-plugging the cable reconnected in 2–15 ms
each time, moving to Wi-Fi and back to Ethernet.

With no network at all, the client polls every 2 s instead of backing off, so it returns
as soon as any network appears.

## Reading it in the logs

Each connect names the path it used:

```
client:  link to 192.168.137.1:24800 goes out through eth0 (192.168.137.2) (candidates tried: …)
server:  input link from 192.168.137.2:51234 arrived through Ethernet (192.168.137.1)
```

After a switch you see `disconnected from server: NetworkChanged` on the client, followed by
a new `goes out through …` line naming the other interface.

## Known limits

- **Client isolation.** Some guest Wi-Fi networks and hotspots block devices from talking
  to each other. Neither beacons nor connections can pass; use another network (B15).
- **Different networks.** The machines must share at least one network. mouse-rs does not
  relay through the internet (B16).
- **IPv4 only** for beacons.
