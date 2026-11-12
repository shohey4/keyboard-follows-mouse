# keyboard-follows-mouse

A non-Logitech keyboard's input follows a Logitech Flow / Easy-Switch mouse
across computers.

You type on a keyboard attached to a Mac. When your Logitech mouse moves to a
Windows PC (by Logitech Flow at the screen edge, or by Easy-Switch), the
keystrokes are sent to that PC over your LAN, encrypted. When the mouse comes
back, the keyboard types on the Mac again.

> **Unofficial.** This project is not affiliated with, endorsed by, or
> supported by Logitech. It does not use Logitech software internals or SDKs;
> it only observes the mouse's behaviour (where the pointer goes, and whether
> the mouse is in the Windows device list). "Logitech", "Logi Options+",
> "Flow", "Easy-Switch" and "MX Master" are trademarks of their owners and are
> used here only to describe compatibility.

## Status — read this first

**Experimental hobby project, provided as is** (see [LICENSE](LICENSE)). It is
not claimed to work in every direction, with every keyboard, or reliably. What
has and has not been verified, per direction (details in
[SPEC.md](SPEC.md#support-matrix)):

| Source → receiver | Implemented | Synthetic-tested | Hardware-tested |
|---|---|---|---|
| Mac → Windows | yes | yes | partly: one session on protocol v4; v5 not yet |
| Mac → Mac | yes | yes (receiver in dry-run) | **not tested**: real injection (CGEventPost) has never run |
| Windows → Mac | **experimental, opt-in** (`kfm-send --experimental-capture-all-keyboards`; captures *every* keyboard on the PC while forwarding) | routing and capture lifecycle only; the Windows hook is compiled, never run | **not tested** |
| Windows → Windows | same binaries; not a target | no | **not tested** |

No Linux.

| | Status |
|---|---|
| Keyboard tested | One keyboard (a Rainy75, wired to the Mac). Others are **not tested**; see [Other keyboards](#other-keyboards). |
| Mouse tested | One Logitech MX Master 4 with Flow and Easy-Switch. Other mice are not tested. |
| Hardware results | One session: 4 round trips Mac ↔ Windows, 8 route changes by Flow / Easy-Switch, no double switch. Long-term stability is **not established** (resident logs on the author's Wi-Fi show frequent short link drops). |
| Protocol v5 | The Ack-echo change that protects the Mac against replayed Acks (see [SECURITY.md](SECURITY.md)) passes the test suite but has **not yet been run on real hardware**. |
| Windows binary | Built and unit-tested by CI on `windows-latest`. `SendInput` injection itself is not covered by automated tests. |

## AI-assisted setup

Clone the repository or use **Code → Download ZIP** and extract it (git is not
needed). Open the folder in Claude Code, Codex or another coding agent, and
paste:

> Set up keyboard-follows-mouse on this computer by following AGENTS.md.
> Tell me the project's status and limits first, use dry runs, and ask me
> before anything that changes my system.

The agent follows [AGENTS.md](AGENTS.md) ([CLAUDE.md](CLAUDE.md) points to it):
it runs `scripts/setup.sh` (macOS) or `scripts\setup.ps1` (Windows), which only
build and diagnose, asks what it cannot detect, writes your settings to
`./local/` with `kfm-config`, and prints each install step with its dry run
and rollback for you to approve. Do this on each computer (the source and the
receiver); the key file is the one thing you copy between them yourself.
Later you can ask it things like "make Caps
Lock a Ctrl on Windows" or "my PC is now above the Mac"; it edits the same
settings with a preview and a backup. You can also run `kfm-config` yourself
(`./target/release/kfm-config --help`).

## How it works

```
 keyboard ──USB──▶ Mac: Karabiner-Elements ──send_user_command──▶ kfm-mac
                                                                     │  UDP, XChaCha20-Poly1305
                                                                     ▼
                     receiver: kfm-receive ──SendInput (Windows) / CGEventPost (macOS)──▶ apps
```

- **Capture (Mac).** Karabiner-Elements grabs the configured keyboard. A
  generated rule (`kfm-karabiner rule`) is placed first in the selected
  profile. While the Karabiner variable `kfm_route` is `1`, the keys in the
  rule's table (see [Which keys are forwarded](#which-keys-are-forwarded)) are
  suppressed on the Mac and forwarded to `kfm-mac` through Karabiner's
  `send_user_command` socket. **Keys outside that table keep acting on the
  Mac.** While `kfm_route` is `0` (or unset) the rule is inactive and the Mac
  behaves exactly as before.
- **Routing (Mac).** `kfm-mac` decides the destination and flips `kfm_route`
  with `karabiner_cli`:
  - **Flow:** the Mac pointer reaches the configured screen edge, and the
    receiver then reports its cursor entering from the facing edge. Set both
    to match your Flow layout: `kfm-mac --flow-edge` (Mac side) and
    `kfm-receive --flow-edge` (receiver side; default `left`), each with
    `--flow-display` to pick displays. Left / right / top / bottom, stepped and
    differently sized displays are handled; see [SPEC.md](SPEC.md#layout-flow).
  - **Easy-Switch:** the mouse appears in / disappears from the Windows PC's
    raw-input device list. `kfm-receive --mx4-id TEXT` names the mouse by a part of
    its device path (the default is the MX Master 4's id); `--mx4-id off`
    disables this. **Windows receivers only**; a macOS receiver follows Flow
    edges alone.
  - **Manual:** F13 on the keyboard toggles. With route 1 it returns to the Mac
    inside Karabiner itself, even if `kfm-mac` is dead. F13 on the configured
    keyboard is always taken by this rule.
- **Transport.** UDP between two pinned addresses. Every datagram is
  authenticated and encrypted with a 32-byte pre-shared key. The Windows side
  releases every held key if nothing authentic arrives for 500 ms.
- **Injection (receiver).** `kfm-receive --enable-inject` uses `SendInput` with
  scan codes on Windows, and `CGEventPost` on macOS (it refuses to start
  without the Accessibility permission). Without `--enable-inject` it only
  counts what it would inject.
- **Key changes.** A keymap file (`KFM_KEYMAP`) can change the switch hotkey
  and remap keys per receiver OS; see [Key configuration](#key-configuration).

## Requirements

- macOS on the keyboard side, with [Karabiner-Elements](https://karabiner-elements.pqrs.org/)
  installed and allowed to grab the keyboard.
- Windows 10/11 on the other side.
- Both machines on the same trusted LAN, able to reach each other by UDP.
- Rust (stable, edition 2024) to build.

Permissions: `kfm-mac` itself needs no Accessibility or Input Monitoring
permission (Karabiner-Elements does the capture). On a Windows receiver,
`kfm-receive` runs as the logged-in user; `SendInput` from a non-elevated
process cannot type into elevated (administrator) windows. On a macOS
receiver, `kfm-receive --enable-inject` needs the Accessibility permission
(System Settings → Privacy & Security → Accessibility). macOS checks the app
that started the process: your terminal when you run it by hand (which then
lets every program run from that terminal post input), or the binary itself
when launchd starts it. Whether macOS keeps injected keys out of password
fields or the lock screen on the receiving Mac is **not verified**; assume
they can arrive there.

## Build

```sh
cargo build --release   # kfm-mac, kfm-karabiner (source Mac); kfm-receive (receiver, Windows or macOS)
cargo test              # unit tests on macOS and Windows; loopback end-to-end tests on macOS
```

## Setup

The examples use documentation addresses (`192.0.2.x`); use your own.

1. **Make a key** on the Mac and copy it to the Windows PC over a channel you
   trust (not chat, not email):

   ```sh
   ./target/release/kfm-config keygen --out ~/kfm-psk.key  # written 0600; same as `kfm-mac keygen`
   ```

   On Windows, keep the key file readable only by your user (the program
   cannot check Windows ACLs and prints a warning).

2. **Install the Karabiner rule.** It installs inactive, and a backup is
   written first:

   ```sh
   ./target/release/kfm-karabiner preflight
   ./target/release/kfm-karabiner apply --config ~/.config/karabiner/karabiner.json \
       --endpoint "/Library/Application Support/org.pqrs/tmp/user/$(id -u)/user_command_receiver.sock" \
       --confirm-backup-and-apply
   ```

   For a keyboard other than the tested one, set `KFM_KEYBOARD_VENDOR_ID` and
   `KFM_KEYBOARD_PRODUCT_ID` for every `kfm-*` command (see
   [Other keyboards](#other-keyboards)).

3. **Start the receiver.** Try it first without `--enable-inject` (it only
   counts what it would type), then add it. Set `--flow-edge` to the side of
   this computer that faces the Mac in your Flow layout (default `left`):

   ```bat
   kfm-receive.exe --bind 192.0.2.10:47501 --peer 192.0.2.20:47502 --key-file C:\path\kfm-psk.key
   kfm-receive.exe --bind 192.0.2.10:47501 --peer 192.0.2.20:47502 --key-file C:\path\kfm-psk.key --enable-inject
   ```

   A Mac receiver (Mac → Mac) is the same binary; also pass `--target-os mac`
   to the source (`KFM_TARGET_OS=mac` for the installer):

   ```sh
   ./target/release/kfm-receive --bind 192.0.2.30:47501 --peer 192.0.2.20:47502 --key-file ~/kfm-psk.key --flow-edge left --enable-inject
   ```

4. **Run the Mac side** as a LaunchAgent. The installer is a dry run by
   default; `--apply` does it:

   ```sh
   KFM_KEY=~/kfm-psk.key KFM_PEER=192.0.2.10:47501 KFM_BIND=192.0.2.20:47502 \
     KFM_FLOW_EDGE=right sh scripts/resident/install.sh            # dry run
   KFM_KEY=... KFM_PEER=... KFM_BIND=... sh scripts/resident/install.sh --apply
   ```

   This installs two LaunchAgents for your user:
   - `local.kfm-mac` (the bridge)
   - `local.kfm-guard` (every 10 s: if `kfm-mac` is not running, force the
     route back to the Mac; rotate logs)

   Files go to `~/Library/Application Support/keyboard-follows-mouse`, logs to
   `~/Library/Logs/keyboard-follows-mouse` (both `0700`). See
   [`examples/config.example.sh`](examples/config.example.sh) for the settings.

Pause, resume or check status with `kfmctl.sh` in the installed `bin` folder.

## Uninstall

```sh
sh scripts/resident/uninstall.sh                  # dry run
sh scripts/resident/uninstall.sh --apply          # stop, set route 0, remove the LaunchAgents
sh scripts/resident/uninstall.sh --apply --purge  # also remove the installed files and the key
./target/release/kfm-karabiner restore --config ~/.config/karabiner/karabiner.json \
    --backup <the backup written by apply> --confirm-restore
```

## Recovery: "my keyboard stopped typing on the Mac"

Any one of these returns the keyboard to the Mac:

1. Press the hotkey on the keyboard: **F13** by default (Fn+F12 on the tested
   keyboard), or the `hotkey` in your keymap.
2. Wait. If `kfm-mac` is gone, the guard sets the route back within about 10 s.
   If the link is down, `kfm-mac` returns to the Mac after 5 s.
3. With the built-in keyboard, run:
   `'/Library/Application Support/org.pqrs/Karabiner-Elements/bin/karabiner_cli' --set-variables '{"kfm_route":0}'`
4. Disable or remove the rule in Karabiner-Elements' settings.

On the Windows side, every held key is released when the link is quiet for
500 ms, when `kfm-receive` stops by Ctrl+C / SIGTERM or `--stop-file`, and when a
new Mac session starts. If `kfm-receive` is killed, crashes or its console window is
closed, a key held at that moment can stay down on the receiver (Windows or
Mac) until you press it again.

## Key configuration

`KFM_KEYMAP=<file.json>` changes the switch hotkey and remaps physical keys
for each receiver OS. Use the same file for `kfm-karabiner apply` and for
`kfm-mac` (the installer copies it); both refuse to start on an invalid file.

```json
{
  "hotkey": "f15",
  "remap": {
    "windows": [["caps_lock", "left_control"], ["left_control", "caps_lock"], ["right_option", "drop"]],
    "mac":     [["left_command", "left_control"], ["left_control", "left_command"]]
  }
}
```

- `hotkey`: one of F13–F20. It is taken from the Mac as well (Karabiner turns
  it into the switch), which is why ordinary keys are not allowed. It stays
  the escape that works inside Karabiner even when `kfm-mac` is dead, so pick
  one your keyboard can always send (map it in the keyboard's firmware if
  needed). After changing it, run `kfm-karabiner apply` again with the same
  `KFM_KEYMAP`; `kfm-karabiner preflight` and the installer refuse a rule
  whose hotkey differs.
- `remap.<os>`: `[from, to]` pairs of key names (as in
  [Which keys are forwarded](#which-keys-are-forwarded)), or `"drop"`.
  `kfm-mac --target-os windows|mac` picks the list. Two keys may not end up as
  the same key (use a swap: `[["caps_lock","left_control"],["left_control","caps_lock"]]`).
  Remove an entry to undo it.
- For a Windows receiver the names are physical keys (Karabiner's swaps for
  the keyboard are undone first); for a Mac receiver they are keys as
  Karabiner reports them (the source Mac's swaps are kept). Dead keys and IME
  composition stay with the receiving OS. Changes that should apply
  everywhere belong in the keyboard's firmware or in Karabiner.

See [SPEC.md](SPEC.md#key-configuration) for the exact rules.

## Other keyboards

The keyboard is selected by its USB vendor and product id, as
Karabiner-Elements reports them (Karabiner-Elements → Devices, or EventViewer):

```sh
export KFM_KEYBOARD_VENDOR_ID=0x1234 KFM_KEYBOARD_PRODUCT_ID=0x5678
```

Keys are mapped from standard USB HID usages, so an ordinary keyboard should
work, but **no keyboard other than the one has been tested**. Requirements and
caveats:

- The keyboard must be able to send the hotkey (**F13** by default, or the one
  you set in the keymap): it is the manual switch and the escape that works
  even when `kfm-mac` is dead.
- Modifier swaps you configured in Karabiner for that keyboard are undone for
  Windows, so Windows receives the physical key.

### Which keys are forwarded

Letters, digits, Return, Escape, Backspace, Tab, Space, the punctuation keys
of a US layout (`` - = [ ] \ ; ' ` , . / ``), Caps Lock, F1–F12, Insert, Home,
Page Up, Delete, End, Page Down, the arrow keys, the JIS Kana / Eisu keys, and
the eight modifiers.

**Not forwarded, and still acting on the Mac while the route is on Windows:**
the numeric keypad, ISO / JIS extra keys other than Kana / Eisu (for example
the ISO `\|` key and international keys), Application/Menu, Print Screen,
Scroll Lock, Pause, F14 and above, and media / consumer keys. Do not type
secrets with those keys while the route is on Windows.

## Limits

- **Secure Input:** while macOS Secure Input is on (password fields, some
  terminals), the route does not move to Windows. The one exception is a stuck
  hold by `loginwindow` while the screen is unlocked.
- **Sleep and network changes:** a sleeping or roaming machine drops the link.
  The Mac falls back after 5 s and Windows releases keys after 500 ms; typing
  in between can be lost.
- **Auto-repeat:** generated by `kfm-mac` (500 ms delay, 33 ms interval), not
  taken from either OS's settings.
- **Pairing:** one Mac and one Windows PC per pair; no discovery, no NAT
  traversal.
- **Mac receiver:** Caps Lock is forwarded as a key event only and does not
  change the receiving Mac's Caps Lock state; auto-repeat is generated by the
  source and posted as ordinary key events.
- **Receiver restart:** after `kfm-receive` restarts, the Mac keeps its old
  session until the link times out (300 ms by default, `--link-timeout-ms`);
  keys typed in that moment are lost, then the link recovers.

- **Windows → Mac (experimental):** `kfm-send` captures *every* keyboard on
  the Windows PC while forwarding and none otherwise; it has never run on
  Windows. Read [SPEC.md](SPEC.md#windows-as-the-source) before using
  `--experimental-capture-all-keyboards`.

## Security

See [SECURITY.md](SECURITY.md) for the threat model, what the protocol defends
against, known limits, and how to report a vulnerability.

## License

MIT — see [LICENSE](LICENSE). Third-party crates are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
