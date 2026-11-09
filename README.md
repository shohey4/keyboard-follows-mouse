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

This is an early, single-setup project. What has and has not been verified:

| | Status |
|---|---|
| Direction | **Mac → Windows only.** No Windows → Mac, no Linux. |
| Keyboard tested | One keyboard (a Rainy75, wired to the Mac). Others are **not tested**; see [Other keyboards](#other-keyboards). |
| Mouse tested | One Logitech MX Master 4 with Flow and Easy-Switch. Other mice are not tested. |
| Hardware results | One session: 4 round trips Mac ↔ Windows, 8 route changes by Flow / Easy-Switch, no double switch. Long-term stability is **not established** (resident logs on the author's Wi-Fi show frequent short link drops). |
| Protocol v5 | The Ack-echo change that protects the Mac against replayed Acks (see [SECURITY.md](SECURITY.md)) passes the test suite but has **not yet been run on real hardware**. |
| Windows binary | Built and unit-tested by CI on `windows-latest`. `SendInput` injection itself is not covered by automated tests. |

## How it works

```
 keyboard ──USB──▶ Mac: Karabiner-Elements ──send_user_command──▶ kfm-mac
                                                                     │  UDP, XChaCha20-Poly1305
                                                                     ▼
                                       Windows: kfm-win ──SendInput──▶ apps
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
    Windows PC then reports its cursor entering from its **left** edge. So
    Flow-follow works only with the Windows PC placed to the **right** of the
    Mac (`KFM_FLOW_EDGE=right`); the Windows-side edge is not configurable.
  - **Easy-Switch:** the mouse appears in / disappears from the Windows PC's
    raw-input device list. `kfm-win --mx4-id TEXT` names the mouse by a part of
    its device path (the default is the MX Master 4's id); `--mx4-id off`
    disables this.
  - **Manual:** F13 on the keyboard toggles. With route 1 it returns to the Mac
    inside Karabiner itself, even if `kfm-mac` is dead. F13 on the configured
    keyboard is always taken by this rule.
- **Transport.** UDP between two pinned addresses. Every datagram is
  authenticated and encrypted with a 32-byte pre-shared key. The Windows side
  releases every held key if nothing authentic arrives for 500 ms.
- **Injection (Windows).** `kfm-win --enable-inject` uses `SendInput` with scan
  codes. Without `--enable-inject` it only counts what it would inject.

## Requirements

- macOS on the keyboard side, with [Karabiner-Elements](https://karabiner-elements.pqrs.org/)
  installed and allowed to grab the keyboard.
- Windows 10/11 on the other side.
- Both machines on the same trusted LAN, able to reach each other by UDP.
- Rust (stable, edition 2024) to build.

Permissions: `kfm-mac` itself needs no Accessibility or Input Monitoring
permission (Karabiner-Elements does the capture). `kfm-win` runs as the
logged-in user; `SendInput` from a non-elevated process cannot type into
elevated (administrator) windows.

## Build

```sh
cargo build --release   # on the Mac: kfm-mac, kfm-karabiner; on Windows: kfm-win.exe
cargo test              # unit tests on macOS and Windows; loopback end-to-end tests on macOS
```

## Setup

The examples use documentation addresses (`192.0.2.x`); use your own.

1. **Make a key** on the Mac and copy it to the Windows PC over a channel you
   trust (not chat, not email):

   ```sh
   ./target/release/kfm-mac keygen --out ~/kfm-psk.key     # written 0600
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

3. **Start the Windows receiver.** Try it first without `--enable-inject` (it
   only counts what it would type), then add it:

   ```bat
   kfm-win.exe --bind 192.0.2.10:47501 --peer 192.0.2.20:47502 --key-file C:\path\kfm-psk.key
   kfm-win.exe --bind 192.0.2.10:47501 --peer 192.0.2.20:47502 --key-file C:\path\kfm-psk.key --enable-inject
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

1. Press **F13** on the keyboard (on the tested keyboard this is Fn+F12).
2. Wait. If `kfm-mac` is gone, the guard sets the route back within about 10 s.
   If the link is down, `kfm-mac` returns to the Mac after 5 s.
3. With the built-in keyboard, run:
   `'/Library/Application Support/org.pqrs/Karabiner-Elements/bin/karabiner_cli' --set-variables '{"kfm_route":0}'`
4. Disable or remove the rule in Karabiner-Elements' settings.

On the Windows side, every held key is released when the link is quiet for
500 ms, when `kfm-win` stops by Ctrl+C or `--stop-file`, and when a new Mac
session starts. If `kfm-win` is killed, crashes or its console window is
closed, a key held at that moment can stay down on Windows until you press it
again.

## Other keyboards

The keyboard is selected by its USB vendor and product id, as
Karabiner-Elements reports them (Karabiner-Elements → Devices, or EventViewer):

```sh
export KFM_KEYBOARD_VENDOR_ID=0x1234 KFM_KEYBOARD_PRODUCT_ID=0x5678
```

Keys are mapped from standard USB HID usages, so an ordinary keyboard should
work, but **no keyboard other than the one has been tested**. Requirements and
caveats:

- The keyboard must be able to send **F13**: it is the manual switch and the
  escape that works even when `kfm-mac` is dead. Map a key to F13 in the
  keyboard's firmware if it has none.
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
- **Receiver restart:** after `kfm-win` restarts, the Mac keeps its old
  session until the link times out (300 ms by default, `--link-timeout-ms`);
  keys typed in that moment are lost, then the link recovers.

## Security

See [SECURITY.md](SECURITY.md) for the threat model, what the protocol defends
against, known limits, and how to report a vulnerability.

## License

MIT — see [LICENSE](LICENSE). Third-party crates are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
