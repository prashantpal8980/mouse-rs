# Launch plan: 26 Oct → 1 Nov 2026

Launch is **Sunday 1 Nov**. Building ends **Thu 29**. **Fri 30** is the owner's review and fine-tuning day.
**Sat 31** is the release build. **Sun 1** is reels and go-live.

Tick a box only when it has been seen working on hardware (the owner's test), not when it compiles.

## Where we are (26 Oct)

Done and signed off on hardware:
- Mouse, keyboard, shortcuts and scroll, Windows ↔ Kali, **both directions**
- Copy/paste of text, images, files and folders, both ways (ladder signed off 14 Oct)
- Login-screen sharing on Linux (B2, 16 Oct)
- Quit means quit, with the transfer warning (R33)
- Network following: Ethernet ↔ Wi-Fi switching with no reconnect by hand (R22)
- Screenshot pasted as a path (B36, fixed 21 Oct)

**Not done.** Each item below has a slot in the daily checklist.

| # | What | Why it matters for launch |
|---|---|---|
| 1 | **Pairing with a code** instead of typing the same long secret on both machines | Today a new user must copy a 32-character secret by hand. That is the first thing every buyer does. |
| 2 | **Connection PIN** (4 digits, like a UPI PIN), on by default, can be changed or turned off | Owner's requirement, 26 Oct |
| 3 | **Lock-screen password** (R32), on by default, can be changed or turned off | Owner's requirement, 15 Oct; parked until now |
| 4 | **B34**: the server never proves who it is | Without this fix, items 2 and 3 can be bypassed by a fake server. It must ship with them. |
| 5 | **Update notice**: "Version x.y is available. Update" | Without it, nobody updates |
| 6 | **Wi-Fi smoothness** | Owner noticed slight lag on 2.4 GHz Wi-Fi, 25 Oct |
| 7 | **Linux ↔ Linux and Windows ↔ Windows** (B33) | Never run. The product says "any direction". |
| 8 | **Release build** | Turn off log shipping, version 1.0.0, installers, README, push |
| 9 | Small items: B35 (copy never marked done), B6 (Windows toast), R34 check, warn on Save? | Polish |

## Decisions the owner needs to make today (26 Oct)

Code for items 1–5 depends on these. A suggested answer is given for each; "yes to all" is fine.

1. **When is the connection PIN asked for?** Suggested: on the first connection after mouse-rs starts,
   and for any new device, but **not** on an automatic reconnect after a Wi-Fi drop or sleep.
   Asking on every drop would be the "annoying" case the owner wants to avoid.
2. **Wrong PIN:** suggested 5 tries, then a 5-minute wait, doubling each time. A 4-digit PIN is only
   safe because the guessing happens on the protected machine with limits; it is never sent or stored as a plain hash.
3. **Lock-screen password defaults** (plan section 8): typed on the controlling machine; pointer
   may move before unlock, clicks and keys may not; covers both the boot login screen and the idle lock screen; after 7 wrong
   tries, locked until someone logs in locally; reset only on the machine itself.
4. **Where do updates come from?** Suggested: a **public downloads-only** GitHub repo
   (no source code), created on 1 Nov. Each release carries a signed `latest.json`. The app checks it at start and
   once a day, then shows a banner. **Update** downloads the new version, verifies its signature, and restarts.
   Nothing runs in the background after Quit (R33).
5. **Paid tier at launch, or free only?** Licensing is **not built**. Suggested: launch free, add paid in v1.1.
6. **Windows "unknown publisher" warning.** There is no code-signing certificate, so SmartScreen will warn on
   first run. There is not enough time left to get one. Suggested: accept it for launch, and add one line to the README telling users to click "More info → Run anyway".

## Owner's decisions, 29 Oct

- **Ethernet first for launch.** The cable is confirmed smooth (B41). Wi-Fi was optimized as far as there was time;
  more Wi-Fi work is v1.1 unless time is left over. Item 6 above is done for launch.
- **Lock-screen password:** the boot login screen is covered by the password (R35, "Ask at the login screen", built
  27 Oct). The idle screen lock moves to v1.1, as the cut list allows.
- **Windows sign-in screen typing:** parked, not in v1.0 (see R41 in UI-REQUIREMENTS).
- VMs run on Kali under virt-manager/KVM (VirtualBox conflicts with KVM on Kali's 6.19 kernel). Mint on the NAT network
  talks to Kali; the Windows VM is on macvtap over eth0 and talks to the laptop.

## Day by day

### Mon 26 Oct (today)
- [ ] Owner: answer the 6 decisions above
- [ ] Claude: **B34 fix**. The server proves it knows the secret too (mutual HMAC), and the certificate is pinned at first pairing.
      *Built 26 Oct: mutual proofs tied to the TLS session (stops a machine in the middle as well).
      The certificate pinning moves to pairing on Tue.*
- [ ] Claude: **Wi-Fi measurement**. Log the round-trip time and jitter of each connection, so the lag shows up as numbers and not just a feeling.
      *Built 26 Oct: one line a minute, "link round trip …" (both sides) and "pointer motion … hitches"
      (on the machine being driven).*
- [ ] Owner, evening: 10 minutes on Wi-Fi, 10 on Ethernet, normal use. The logs ship themselves.
- [ ] Owner: check that the laptop's RAM and disk can hold a Windows VM (8 GB+ free RAM is best). Download the Windows 11 evaluation ISO
      and VirtualBox on both machines. The big downloads should run overnight.

### Tue 27 Oct: pairing + PIN
- [ ] Claude: **pairing with a code**. The client lists servers it found on the network. The server shows a
      6-digit code once, and the user types it on the client. The secret is exchanged with a PAKE (SPAKE2), so the code can't
      be guessed by someone listening on the network. The typed shared secret stays under Advanced for existing setups.
      *Built 27 Oct (R38): SPAKE2 over TLS, key confirmation bound to the TLS session, code works once and dies
      after 3 wrong tries; the client tries every server it finds when no address is set.*
- [ ] Claude: **connection PIN**. It is set on the server, typed on the client, and checked on the server, with try limits.
      Settings → Security: Change PIN / Turn off (turning it off needs the current PIN).
      *Built 27 Oct with the suggested answers to decisions 1–2 (see R35 in UI-REQUIREMENTS). Open points in B40.*
- [ ] Claude: **Wi-Fi fix** from Monday's numbers. Most likely: merge mouse moves that queue up behind
      a slow Wi-Fi packet into one move, so the pointer jumps to where it should be and doesn't replay old moves late.
- [ ] Owner, evening: pair the laptop and Kali from scratch (Clear settings → pair with the code), PIN on/off/change,
      wrong PIN 6×, then a Wi-Fi feel test.

### Wed 28 Oct: lock-screen password + updates
- [ ] Claude: **lock-screen password** (R32) with the agreed defaults, plus Change / Turn off in Settings → Security.
- [ ] Claude: **update notice**: version check, banner, Update button, signature check, swap and restart,
      on both Windows and Linux. Tested against a local fake release server, because the public repo doesn't exist yet.
      *Built 29 Oct (R36). Signed latest.json, download checked by size + SHA-256, swap, restart. Unit tests cover
      the signature, versions, a good/bad/short download; a test1 build found test2 on a local server and showed the
      banner. Rehearsal staged: `scripts/fake-release.sh`.*
- [ ] Claude: **B35** (small), **B6**: confirm the Windows toast.
      *B35 fixed in code 29 Oct. B6 is a look on the laptop, in the owner's test.*
- [ ] Owner, evening: reboot Kali, then unlock the login screen from the laptop with the password, a wrong password, and with the password turned off;
      run an update from 1.0.0-test1 to 1.0.0-test2 on both machines.

### Thu 29 Oct: same-OS pairings + fresh install (B33)
- [ ] Owner + Claude: **Linux ↔ Linux**. A Kali or Ubuntu VM in VirtualBox on Kali, Bridged network: pairing, mouse,
      keys, copy text, files and a folder, both roles.
- [ ] Owner + Claude: **Windows ↔ Windows**. A Windows 11 VM on the laptop, Bridged network, same list.
- [ ] Owner: **fresh-install test** on a VM, as a new customer would do it: download, run, pair, use. With no static IP,
      using Wi-Fi/DHCP only, discovery must find the other machine by itself.
- [ ] Claude: fix whatever these turn up the same day.

### Fri 30 Oct: owner's fine-tuning day
- [ ] Owner: use it for real all day. Anything annoying or unacceptable gets written down and fixed today.
- [ ] Claude: fixes only. **No new features from this point.**

### Sat 31 Oct: release build
- [ ] Claude: version **1.0.0**, CHANGELOG, README (install, pairing, SmartScreen note, Linux packages needed).
- [ ] Claude: **turn off log shipping** in the release build, and check that no test address (192.168.137.x) is built in.
- [ ] Claude: build the Windows zip and Linux tarball, sign `latest.json`, compute checksums, test-install both from the files.
- [ ] Claude: scan for secrets, then push the source to the private repo.
- [ ] Owner: final OK.

### Sun 1 Nov: launch
- [ ] Owner's go: create the public downloads repo and publish release 1.0.0. The update check now points at it.
- [ ] Reels: tip: **screen-record during the Tue–Fri tests** (pairing, copy a folder across, lock-screen unlock,
      Wi-Fi ↔ Ethernet switch), so Sunday is just editing, not filming.

## If something slips

Cut in this order, moving the item to v1.1:
1. Wi-Fi smoothness beyond the measurement and the move-merging fix
2. Lock-screen password on the idle **screen lock** (keep it for the boot login screen)
3. Linux in-app update (show the banner, but link to the download instead of updating in place)

These never move to v1.1: B34, pairing, connection PIN, update notice, the same-OS tests, and turning log shipping off.
