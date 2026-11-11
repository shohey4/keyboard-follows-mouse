# Specification

What the code does today, what it does not, and why. "Source" is the
computer the physical keyboard is attached to; "receiver" is the computer the
mouse is currently on.

## Support matrix

Experimental, as-is software. Each direction is rated on three separate
questions; "not tested" is stated, not hidden, and is not a reason to withhold
the code.

| Source → receiver | Implemented | Synthetic-tested | Hardware-tested |
|---|---|---|---|
| Mac → Windows | Yes (`kfm-mac` → `kfm-receive`). | Yes: unit, synthetic end-to-end, and the binaries over UDP. | Partly: one session with one keyboard and one mouse on protocol v4. v5 has not run on hardware. |
| Mac → Mac | Yes (`kfm-mac --target-os mac` → `kfm-receive` on the other Mac). | Routing, protocol and key mapping; the receiver runs in dry-run. | No. **CGEventPost injection has never run** (no hardware, no automated test); needs the Accessibility permission. Planned once a second Mac is available. |
| Windows → Mac | **Experimental, opt-in** (`kfm-send --experimental-capture-all-keyboards` → `kfm-receive` on the Mac). See [Windows as the source](#windows-as-the-source). | The capture lifecycle and routing (pure state machine). The Windows hook and hotkey code is compiled for Windows only, never run. | No. The next hardware target; enabling capture is done by the owner, never unattended. |
| Windows → Windows | The same two binaries could be paired; not a target. | No. | No. |

One source and one receiver per pair. Three or more computers, several
keyboards at once, and Linux are not supported.

## Components

| Part | Where | OS-specific? |
|---|---|---|
| Capture and suppression of one keyboard | Karabiner-Elements rule (`kfm-karabiner`) + `kfm-mac` | macOS only |
| Routing (where keys go, link-loss policy, hotkey) | `router`, `karabiner::Bridge` | no |
| Key mapping (Karabiner swaps undone, user remaps) | `keymap`, `remap` | no |
| Wire protocol, replay defence, key-state sync | `protocol`, `link`, `transport` | no |
| Flow / Easy-Switch inference | `flow` | no (inputs come from the binaries) |
| Windows source: capture while forwarding (experimental) | `kfm-send` + `winsource` (pure lifecycle) | Windows only |
| Receiver: injection | `kfm-receive`: SendInput (Windows), CGEventPost via `macinject` (macOS) | yes |
| Receiver: cursor, displays, mouse presence | `kfm-receive` | yes (mouse presence: Windows only) |

## Wire protocol (v5)

- Frame: 44-byte header (magic `KFM`, version 5, direction, 16-byte session
  id, 8-byte counter, 16-byte challenge) as associated data, then
  XChaCha20-Poly1305 ciphertext. Pre-shared 32-byte key.
- Source → receiver: the newest key events plus a snapshot of every held key.
  Keys travel as Windows set-1 scan codes (or a virtual key for the IME keys);
  a macOS receiver maps them back to the physical key and then to a macOS
  keycode, so the same wire format serves both receiver OSes.
- Receiver → source (Ack): what was applied, mouse activity, edge events at
  the configured edge, mouse presence counts, and the source session it last
  accepted (`echo`).
- Defences: see SECURITY.md. v5 is not compatible with v4; both sides must be
  updated together, and rolling back means rolling back both.

## Key configuration

One JSON file named by `KFM_KEYMAP`, read by `kfm-karabiner` (it generates the
capture rule) and by `kfm-mac` (it sends). Both refuse to start on an invalid
file.

```json
{
  "hotkey": "f13",
  "remap": {
    "windows": [["caps_lock", "left_control"], ["left_control", "caps_lock"], ["right_option", "drop"]],
    "mac":     [["left_command", "left_control"], ["left_control", "left_command"]]
  }
}
```

- `hotkey`: the manual switch, and the escape that works inside
  Karabiner-Elements even when `kfm-mac` is dead. One of F13–F20 (it is also
  taken from the Mac, so ordinary keys are refused). Changing it requires
  re-applying the Karabiner rule with the same `KFM_KEYMAP`; `kfm-karabiner
  preflight` (and so the installer) reports a rule with another hotkey.
- `remap.windows` / `remap.mac`: key → key on that receiver OS
  (`kfm-mac --target-os windows|mac` picks the list), or `"drop"`. For
  Windows the keys are physical (Karabiner's modifications undone first); for
  a Mac receiver they are the keys as Karabiner reports them. Resolved at
  key-down so a held key's key-up always matches.
- Rejected: unknown names, a key listed twice, two keys that would end up as
  the same key, the hotkey as a source, unknown fields. An empty
  `KFM_KEYMAP` counts as unset.
- Not a layout engine: dead keys and IME composition stay with the receiving
  OS. Keyboard firmware (VIA / QMK) and Karabiner simple modifications act
  before this; use them for changes that should apply on every computer.
- Removing an entry (or the file) restores the default mapping.

## Layout (Flow)

The source watches one edge of its own displays (`kfm-mac --flow-edge`,
`--flow-display`); the receiver watches the facing edge of its displays
(`kfm-receive --flow-edge`, `--flow-display`, default left / all). Only an
*outer* edge counts: a border shared with another display of the same
computer never does, so stepped and differently sized displays work. Any of
left / right / top / bottom can be configured on either side, which covers
the arrangements Logi Options+ Flow lets you draw between two computers.

What is not claimed:

- The layout is not read from Logi Options+ and nothing is written to it; you
  describe the same layout in both configurations.
- Only one receiver per pair: two computers on the same edge cannot be told
  apart, so that arrangement is not supported.
- Decisions need both signals (source edge, then receiver entry) or the mouse
  appearing in the receiver's device list (Easy-Switch). The device-list
  signal is Windows-only; a macOS receiver relies on the edge signals alone.

## Safety properties

- Switching to the receiver needs a confirmed link; link loss holds keys and
  falls back to the source after 5 s (resident) while the receiver releases
  every held key after 500 ms of silence.
- Secure Input on the source Mac keeps keys on the Mac.
- Receiver injection is off unless `--enable-inject`. On macOS it is refused
  when the process lacks the Accessibility permission (macOS checks the app
  that started it). Injected events are marked (Windows `dwExtraInfo`, macOS
  `kCGEventSourceUserData`). A receiver Mac's own Karabiner rule matches by
  device (`device_if`), so posted events are not expected to be captured again;
  this has not been verified on hardware.
- Windows does not deliver injected keys to elevated windows from a
  non-elevated receiver. Whether macOS keeps injected keys out of secure input
  fields or the lock screen is **not verified**; the macOS receiver has no
  lock-screen guard of its own.
- macOS receiver events carry the modifier flags (with left/right bits) and
  the Fn / numeric-pad flags real arrow and function keys carry. Caps Lock
  does not change the receiving Mac's lock state.

## Windows as the source

Forwarding a keyboard attached to a Windows PC requires two things: knowing
which physical keyboard a key came from, and stopping *only that keyboard's*
keys from also typing on the Windows PC.

- Raw Input (`RegisterRawInputDevices`, `RIDEV_INPUTSINK`) tells which device a
  key came from, but cannot stop the key from reaching applications.
- A low-level keyboard hook (`WH_KEYBOARD_LL`) can stop keys, but does not know
  which device they came from. Pairing it with Raw Input by timing is a known
  heuristic without an ordering guarantee, so it can swallow or leak keys of
  the wrong keyboard.
- A kernel keyboard filter driver can do both, but installing a third-party
  input driver is outside what this project will ask users to do.

So per-keyboard capture on Windows is not implemented. Instead `kfm-send`
offers an **experimental mode that captures every keyboard on the PC, and only
while forwarding**:

- **Opt-in.** Without `--experimental-capture-all-keyboards`, `kfm-send` runs
  the link and the Flow logic in dry-run: it never installs a hook and never
  suppresses a key.
- **No hook while not forwarding.** In normal use the PC is untouched: there is
  no keyboard hook at all, and the manual switch key (F13–F20, the `hotkey` of
  the key configuration) is a `RegisterHotKey` registration. The
  `WH_KEYBOARD_LL` hook is installed when forwarding starts and removed on
  every way out of it.
- **Starting** needs a confirmed link: the hotkey, or Flow (the cursor leaves
  the configured edge and the Mac reports the pointer entering).
- **Leaving:** the hotkey (recognised inside the hook before anything else,
  also while the link is down); the mouse coming back to the PC (with
  `--flow-edge`, the same Flow rule as `kfm-mac`; without it, the cursor moving
  60 px or more from where capture began, so press the hotkey once the pointer
  is already on the Mac); link loss (keys are held, then capture stops after
  `--link-loss-stop-ms`, default 3 s); no forwarded key for `--idle-stop-secs`
  (default 10 minutes); the secure desktop appearing (lock screen, UAC,
  Ctrl+Alt+Del); closing the console window with the mouse. Ctrl+C does
  **not** work while forwarding (the hook sends Ctrl and C to the Mac); it
  works only when not forwarding. If the process dies, Windows removes the
  hook itself and the Mac releases its keys after 500 ms of silence. Keys
  still held on the Mac are released on every way out.
- **Every keyboard** on the PC is captured while forwarding, including a
  laptop's built-in one. Keys without a wire equivalent (numeric keypad,
  Pause, some JIS keys) are swallowed, not typed locally. Events injected by
  software pass through untouched.
- **Nothing is recorded.** The binary prints state changes and counters, never
  key codes or characters. At the start of forwarding it reads which keys are
  already down (so their release still reaches Windows) and forgets each one
  at its release; this is never written anywhere.

Known limits (not verified on hardware):

- The hook does not run on the secure desktop (lock screen, UAC prompt,
  Ctrl+Alt+Del), so keys typed there stay on the PC; forwarding stops when it
  appears. Win+L cannot be blocked by a hook, so it locks the PC even while
  forwarding.
- AltGr's synthetic Ctrl has no wire equivalent and is dropped, so the Mac
  receives Right Option alone.
- Software that injects F13–F20 can toggle forwarding through the registered
  hotkey.
- Windows silently removes a low-level hook whose callback is too slow
  (`LowLevelHooksTimeout`); the program cannot detect that, and keys then type
  on the PC again. The hotkey stays registered while forwarding, so pressing
  it still returns to local in that case.
- There is no counterpart of macOS Secure Input: a password typed while
  forwarding goes to the Mac.
- Behaviour with an elevated (administrator) window in front, and whether other
  Raw Input readers still see the captured keys, are not verified.

Still not done: per-keyboard capture (a firmware-side switch such as VIA / QMK
layers would be the clean way), and Windows → Windows.
