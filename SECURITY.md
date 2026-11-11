# Security

keyboard-follows-mouse turns network datagrams into keystrokes on a Windows PC.
Treat it as a remote-input service, and run it only on a LAN you trust.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting on this repository
(**Security → Report a vulnerability**). Do not open a public issue with
details. There is no bug bounty, and response time is best effort.

## Threat model

Assets: the keystrokes you type (which may include passwords), and control of
the Windows PC's keyboard input.

In scope: an attacker on the same LAN who can observe, record, drop, delay,
reorder, replay and forge UDP datagrams, but does not have the pre-shared key.

Out of scope: malware running as your user on either machine (it can already
read or inject keystrokes locally), anyone holding the key file, and physical
access.

## What the protocol does

- **Pre-shared key.** 32 random bytes. `kfm-config keygen` (or `kfm-mac keygen` on a Mac) writes the key file
  with mode `0600`, and both binaries refuse a key file that is readable by
  others on Unix. On Windows the ACL is **not checked** (a warning is
  printed); protect the file yourself.
- **Encryption and authentication.** Every datagram is XChaCha20-Poly1305. The
  header (protocol version, direction, session id, counter, challenge) is
  authenticated as associated data, so it cannot be altered or reflected in
  the other direction.
- **Nonces.** Each process picks a random 128-bit session id per run; nonce =
  session id ‖ 64-bit counter, so a nonce is never reused under the same key.
- **Replay towards Windows.** Counters must strictly increase within a
  session, the last 64 sessions are refused, and every Mac→Windows frame must
  carry the receiver's current random challenge. Frames recorded before a
  receiver restart are therefore dead.
- **Replay towards the Mac (protocol v5).** Every Ack echoes the Mac session it
  last accepted, and the Mac trusts only Acks that echo its current session.
  Acks recorded earlier cannot make the Mac believe the link is up (which would
  otherwise send your typing to a receiver that discards it). A hello that does
  not echo the current session can only tell the Mac which challenge to use,
  and only while no verified Ack is fresh. The Mac does not retire Windows
  sessions (it keeps each recent session's last counter instead), so one
  replayed Ack from an old receiver run cannot lock out the live session. These
  changes are covered by tests but have not yet been run on real hardware.
- **Pinned peers.** Each side accepts datagrams only from the one configured
  peer address. This is not authentication (addresses can be spoofed on a
  LAN); the key is.
- **Fail-safe key release.** The Windows side releases every held key when no
  authentic datagram arrives for 500 ms, when it stops by Ctrl+C or
  `--stop-file`, and when a new Mac session starts (not when it is killed or
  crashes). Held-key snapshots in every datagram keep both sides
  consistent despite loss.
- **Fail-safe routing (Mac).** The route moves to Windows only after the link
  is confirmed. The hotkey (F13 by default) returns to the Mac inside Karabiner even if `kfm-mac` is
  dead, and the guard LaunchAgent resets the route when `kfm-mac` is not
  running.
- **Secure Input.** While macOS Secure Input is on, the route is not moved to
  Windows.
- **Logs.** Key names and contents are never logged; only times, counts and
  error kinds. The installer keeps the log folder `0700`.

## Known limits

- No forward secrecy: anyone who later obtains the key can decrypt recorded
  traffic. Rotate the key (`kfm-config keygen`, reinstall on both sides) if a
  machine or the key file may have been exposed.
- Denial of service: an attacker on the LAN can drop traffic or flood the UDP
  port. The link then goes down and the keyboard returns to the Mac; typing in
  that moment can be lost. There is no rate limit.
- Keys outside the forwarded table (numeric keypad, media keys and others; see
  README) keep acting on the Mac while the route is on Windows.
- Local processes running as your user can write to Karabiner's
  `send_user_command` socket and so inject keys that `kfm-mac` forwards while
  the route is on Windows. This is the same trust level as any local input
  method.
- `SendInput` cannot type into elevated windows from a non-elevated
  `kfm-receive`, and some applications ignore injected input.
- On a macOS receiver, `kfm-receive --enable-inject` needs the Accessibility
  permission. macOS grants it to the app that started the process: when you
  run it from a terminal, the terminal needs it, and then every program run
  from that terminal can post input. Prefer starting it from launchd so the
  grant belongs to the binary alone.
- On a macOS receiver it is **not verified** whether injected keys are kept
  out of password fields or the lock screen; assume keys you type can arrive
  there, and do not leave a receiving Mac locked with the link up.
- An invalid keymap or layout setting stops the binaries before they listen
  (fail closed); a valid but unwanted remap is applied as written.
- Sleep, Wi-Fi roaming and address changes break the link; there is no
  discovery or re-pairing.
- `kfm-send --experimental-capture-all-keyboards` (Windows source) installs a
  system-wide low-level keyboard hook while it forwards. During that time it
  sees and suppresses **every** keyboard on the PC, and anything typed goes to
  the Mac, including passwords (Windows has no counterpart of Secure Input).
  It records nothing, and the hook is removed when forwarding ends, but treat
  it like any program that holds a keyboard hook. It has not run on Windows
  yet.

## Things you should not do

- Do not expose the UDP ports to the internet or to untrusted networks.
- Do not commit or share the key file; do not paste it into chat or email.
- Do not leave `kfm-send` forwarding unattended, and do not run it as an
  administrator.
- Do not run `kfm-receive` with `--enable-inject` as an administrator unless you
  understand that remote keystrokes then reach elevated windows.
