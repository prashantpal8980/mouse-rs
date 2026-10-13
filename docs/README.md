# Documentation index

## Start here

| Document | What it covers |
|---|---|
| [../README.md](../README.md) | What mouse-rs is, building, configuring, running |
| [KNOWN-ISSUES.md](KNOWN-ISSUES.md) | The standing bug list (B1…), with evidence and state |
| [UI-REQUIREMENTS.md](UI-REQUIREMENTS.md) | Agreed behaviour of the control panel (R1…) |
| [STATUS.md](STATUS.md) | Longer project history and phase status |
| [PLAN-SETTINGS-AND-LAUNCH.md](PLAN-SETTINGS-AND-LAUNCH.md) | Settings reorganisation, launch-readiness list for 1 Nov, copy/paste work |

## How it works

| Document | What it covers |
|---|---|
| [NETWORKING.md](NETWORKING.md) | Ports, discovery beacons, racing connects, cable ↔ Wi-Fi failover |
| [LOGIN-SCREEN.md](LOGIN-SCREEN.md) | The uinput login-screen service: why, how, install, hand-over |
| [phase4-windows-design.md](phase4-windows-design.md) | Windows input backend design (Raw Input, SendInput) |
| [KEYBOARD-LEAK.md](KEYBOARD-LEAK.md) | Root cause and fix of keys leaking on Windows |
| [PACKAGING.md](PACKAGING.md) | Build variants, runtime dependencies, cross-compiling, archives |

## Testing and operations

| Document | What it covers |
|---|---|
| [TESTING.md](TESTING.md) | Two-machine test runbook |
| [windows-update-prompt.md](windows-update-prompt.md) | Standing procedure for installing a new build on Windows |
| [windows-rollback-prompt.md](windows-rollback-prompt.md) | Rolling Windows back to a previous build |
| `windows-*-prompt.md` (others) | Earlier, one-off Windows test rounds |

## Post-mortems and explainers

| Document | What it covers |
|---|---|
| [POSTMORTEM-2026-10-09-modifier-regression.md](POSTMORTEM-2026-10-09-modifier-regression.md) | The day Ctrl stopped working |
| [postmortem-explained.html](postmortem-explained.html) | Visual version of that post-mortem |
| [login-screen-explained.html](login-screen-explained.html) | Visual explainer for the login-screen service |
| [slow-transfer-explained.html](slow-transfer-explained.html) | Why a large RAR transfer crawled, and the fix |
| [mockups/](mockups/) | UI prototypes and planning diagrams |
| [../status-mockup/](../status-mockup/) | Dated visual progress reports |

## History

`RESUME-*.md` and `engine-resume.md` are dated resume points written during development.
They describe the state at the time and are kept for history; prefer the documents above
for the current state.
