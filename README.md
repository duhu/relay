# Relay

**Software KVM for Macs sharing one monitor and one Logitech keyboard + mouse.**

[简体中文](README.zh-CN.md)

You press **Easy-Switch 2** on the keyboard. The keyboard hops to the other Mac by
itself — and Relay sends everything else after it: the monitor changes input source
over DDC/CI, and the mouse changes host over HID++. About a second later the other
Mac has the screen, the keyboard and the mouse.

No extra box on the desk, no cable to unplug, no software on a server. Each Mac runs
its own copy of Relay and talks to the hardware it already owns.

Relay lives in the menu bar. It opens no window until you ask for Settings.

---

## How it works

Every Mac watches its own USB/Bluetooth HID devices. The one that *loses* the
keyboard is the one that acts:

```
Mac A                                           Mac B
  │
  │ keyboard disappeared from this Mac
  ▼
  debounce 800 ms  ─ still gone? then go ─┐
                                          │
  ├── DDC/CI   VCP 0x60 := 18  ───────────┼──▶  monitor now shows Mac B
  └── HID++    ChangeHost := host 2  ─────┘     mouse now typing into Mac B
                                                  │
                                                  │ keyboard arrived here
                                                  ▼
                                          ask the monitor where the picture is
                                          (VCP 0x60 read) — if it is elsewhere,
                                          pull the screen and the mouse back
```

Two rules make this safe with more than one Relay running:

- **Only the departure side pushes.** A Mac acts on *its own* keyboard leaving, never
  on a guess about the others.
- **The arrival side asks the monitor.** When the keyboard shows up, that Mac reads
  the monitor's current input source before doing anything. If the picture is already
  here, it does nothing. This is what makes it work when the other Mac is asleep, or
  never saw the keyboard leave at all.

A state machine (`Idle → Confirming → Switching → Cooldown`) serialises everything and
enforces a cooldown. That cooldown is not cosmetic: sending `ChangeHost` in a tight
loop has been observed to wedge the Bluetooth stack until reboot.

## Requirements

| | |
|---|---|
| **Macs** | Apple Silicon, macOS 14 or newer. Intel is not supported. |
| **Monitor** | An external display that accepts DDC/CI writes on VCP `0x60` (input source). Reading `0x60` back is optional but makes Relay smarter. Built-in displays cannot be switched. |
| **Keyboard / mouse** | Logitech devices that implement HID++ 2.0 `ChangeHost` (`0x1814`) — i.e. anything with Easy-Switch keys. Bluetooth LE, Bolt and Unifying all work. |
| **Permission** | Input Monitoring, so Relay can see the keyboard come and go. Granted once, in System Settings. |

The Easy-Switch channel numbers on your devices and the input sources on your monitor
are what you map to each other in Settings. Nothing is hardcoded and nothing is
detected by brand name.

## Install

There is no signed release yet — Relay uses private IOKit symbols and Input
Monitoring, so it will never be on the App Store. Build it yourself:

```bash
git clone git@github.com:duhu/relay.git
cd relay
pnpm install
pnpm tauri build
```

Then install and sign it. Signing with a real identity matters: an ad-hoc signature
works, but macOS ties the Input Monitoring grant to the signature, so you would have
to grant it again after every rebuild.

```bash
cp -R target/release/bundle/macos/Relay.app /Applications/
codesign --force --sign "Apple Development: your@email" --identifier work.bam.relay /Applications/Relay.app
ln -sf /Applications/Relay.app/Contents/MacOS/relay ~/.local/bin/relay
open -a /Applications/Relay.app
```

Build prerequisites: Rust 1.98+, pnpm, and the Xcode command line tools.

## First run

A Mac with no configuration opens its own window and asks whether this is the first
Mac you are setting up or whether another one is already configured. Nothing is
written to disk until the end.

**The first Mac — four steps:**

1. **Input Monitoring** — granted in System Settings; without it Relay cannot see the
   keyboard come and go.
2. **Keyboard and mouse** — Relay scans and you pick one **trigger** (the keyboard:
   its leaving is what starts a switch) and one **follower** (the mouse: it gets sent
   along). The devices also report which Easy-Switch channel they are on, which is how
   Relay knows which Mac this one is.
3. **The Macs** — name each Mac that shares the keyboard. The channel number is
   literally the Easy-Switch key: the Mac you reach with key 2 is channel 2.
4. **The display** — the shared monitor, then the input source each Mac shows on,
   picked by name from what the monitor reports — "HDMI 1", "DisplayPort 2". The one
   on screen right now is marked, so the Mac you are sitting at can point at its own
   port. A monitor that will not report its inputs leaves a number box to type the
   code into. You can skip this step and add the monitor in Settings later.

At the end Relay offers to **export** the config. Take that file to the next Mac.

**Every other Mac — import that file:** install Relay, start it, answer that another
Mac is already set up, and choose the file. It asks which of these Macs this one is
(the keyboard usually already knows) and, with three Macs, where a switch away from
this one should go. Everything else — names, channels, input sources, devices,
timings — comes from the file unchanged.

Afterwards everything is in **Settings**, from the menu bar icon: **Overview** (where
the screen, keyboard and mouse are, a manual switch per Mac, the last switch step by
step), **Macs & displays**, **Keyboard & mouse**, and **Advanced** (debounce, cooldown,
retries, behaviour switches, export/import, and **Run the wizard again**). Config lives
at `~/Library/Application Support/Relay/config.json` and is reloaded when it changes.

The interface is available in English and Simplified Chinese, following the system
language unless you pick one in Advanced.

## Command line

The same binary is the CLI. It talks to the running app over a unix socket and starts
it if it is not up.

```
relay switch <host|next>    switch to a host slot, or to the one after this machine
relay status                state, config, permission, last result
relay --settings            open the settings window
relay --devices             Logitech HID devices this Mac can see
relay --displays            external displays this Mac can drive
```

Exit codes: `0` ok, `1` failed, `2` usage, `3` the app is not running.

## What it cannot do

- **Non-Logitech input devices**, and wired ones. `ChangeHost` is the whole mechanism.
- **Intel Macs and built-in displays.**
- **Monitors that only accept writes on `0x60`.** Relay still switches them; it just
  falls back to remembering where it last sent the picture instead of asking.
- **Bring the mouse back when nobody saw it leave.** With three or more machines, the
  Mac holding the mouse cannot know which of the others the keyboard went to, so the
  mouse can be left behind — one normal round trip brings it home. The real fix is to
  let the machines tell each other, which is not built yet.
- **Sync your config.** Each Mac is set up by hand today.

## Architecture

- [`docs/overview.md`](docs/overview.md) — module map, process layout, and the
  cross-module invariants that must hold (in Chinese).
- [`docs/specs/relay-core.md`](docs/specs/relay-core.md) — the design record: every
  decision with the reason behind it, the config format, the state machine, and the
  DDC and HID++ wire details (in Chinese).

In short: `crates/relay-core` is plain Rust with no Tauri dependency and a sans-IO
state machine, so nearly all of it is unit-testable without hardware. `src-tauri` is
the app shell, tray and IPC. `src` is a small Vue 3 frontend with no component
library.

```bash
cargo test --workspace && cargo clippy --workspace --all-targets && pnpm build
```

## Acknowledgements

- **[m1ddc](https://github.com/waydabber/m1ddc)** (MIT) — the DDC-over-`IOAVService`
  approach for Apple Silicon. Relay reimplements it in Rust rather than bundling it.
- **[`openlogi-hid`](https://crates.io/crates/openlogi-hid)** and
  **[`openlogi-hidpp`](https://crates.io/crates/openlogi-hidpp)** — HID enumeration,
  transport, and the HID++ 2.0 protocol including `ChangeHost`.

## License

MIT. See [LICENSE](LICENSE).
