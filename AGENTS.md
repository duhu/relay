# AGENTS.md

## Before a complex task

Read [`docs/overview.md`](docs/overview.md) first: architecture, module map, cross-module
invariants. Design decisions and the reasoning behind them live in
[`docs/specs/relay-core.md`](docs/specs/relay-core.md). Do not edit the overview unless the
repo-wide map or an invariant actually changed.

## Hardware constraints (read before touching device code)

This project drives real hardware that can be left in a bad state. These are not style rules.

- Never call `set_current_host` on a host slot that is not declared in `config.json`.
  An undeclared slot may be unpaired; the device would disconnect and not come back.
- Never keep a HID++ channel open longer than one switch operation. Open → act → close.
  Holding it fights with Logi Options+ over the same device.
- Never act on a presence event without going through `SwitchCoordinator`'s debounce and
  cooldown. Repeated `ChangeHost` calls have wedged the Bluetooth stack badly enough to need
  a reboot: the mouse stutters and the Mouse pane disappears from System Settings.
- Host indices are 0-based internally (HID++ convention). The UI shows index + 1.

## Verifying your changes

```bash
cargo test --workspace && cargo clippy --workspace --all-targets && pnpm build   # must be warning-free
./scripts/dev-install.sh          # build --debug, install /Applications/Relay-dev.app, sign, symlink ~/.local/bin/relay
relay --status                    # daemon/core status (state, config_ok, input_monitoring)
relay --devices                   # Logitech HID devices this Mac sees (vid:pid name) — no daemon needed
relay --displays                  # external displays with a DDC channel (edid_uuid name) — no daemon, no permission
relay switch 1 --dry-run          # prints the plan without touching hardware
```

Exit codes: 0 ok · 1 failed · 2 usage · 3 resident app not running (`--no-spawn`).

An agent working on this repo shares the monitor and the keyboard with the person running it:

- Before any `relay switch`, any DDC write, or any reinstall, run `relay --devices` on every
  Mac involved. Switch only *towards* the Mac the person is sitting at, and reinstall a Mac
  only while they are on a different one. Yanking the display away mid-session is easy to do
  by accident and annoying to recover from.
- `cargo run -p relay-core --example ddc -- set first 17` writes to the monitor for real.
- Do not run `cargo run -p relay-core --example scan`, or anything else that opens a HID++
  channel, from an agent shell: it has no Input Monitoring grant and will report nothing
  useful. Exercise device paths through the installed app instead.

Hardware tests (`docs/specs/relay-core.md` §9) are manual. There is no way around that: the
unit tests cover the state machine and the protocol encoding, not the hardware.

## Code comments & commits

Comments in English. Conventional Commits: `feat|fix|perf|refactor|docs|test|chore(scope): subject`,
scopes: `core`, `hidpp`, `ddc`, `trigger`, `tray`, `settings`, `cli`, `config`, `i18n`.
