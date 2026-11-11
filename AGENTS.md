# Instructions for AI coding agents (Codex, Claude Code, others)

You are helping someone set up or reconfigure **keyboard-follows-mouse** on
their own computer. They may have cloned the repository or extracted the
GitHub ZIP (then there is no `.git`; do not rely on git). Work only from this
folder, this file and README.md; assume nothing about the person's machine.

## 0. Say this first

Before doing anything, tell the user in plain words:

- This is an **experimental hobby project, provided as is**. Read the status
  table in README.md ("Status — read this first") and summarise it. In short:
  Mac → Windows is the only direction that has run on hardware; Mac → Mac has
  never run real injection; Windows → Mac is an experimental opt-in mode that
  has never run on Windows; Windows → Windows is not a target. Never describe
  an untested direction as working.
- What the software does to a computer: on the source Mac it captures one
  keyboard through a Karabiner-Elements rule; on the receiver it types the
  forwarded keys (and on macOS that needs the Accessibility permission); the
  experimental Windows source captures **every** keyboard on that PC while
  forwarding. Keys travel encrypted over the LAN (UDP).

## 1. Ground rules

- **Nothing that changes the system without the user's explicit yes, given
  for that step**: installing tools, Karabiner rules, LaunchAgents/residents,
  OS permissions (Accessibility, Input Monitoring), firewall rules, starting a
  receiver with `--enable-inject`, starting `kfm-send` with
  `--experimental-capture-all-keyboards`. Before asking, explain what the step
  does, what could go wrong, and how to undo it. Building into `./target` and
  editing files under `./local/` are fine without asking.
- **Secrets stay on the machine.** Never print, paste or upload the key file,
  and do not put LAN addresses or the key path into chat, issues, commits or
  any external service. `kfm-config show` masks them; use `--reveal` only if
  the user asks to see them locally. The key is made locally
  (`kfm-mac keygen`) and copied to the other computer by the user over a
  channel they trust (USB stick, AirDrop, a local file share), never through
  you.
- **Do not overwrite an existing installation.** `kfm-config doctor` reports
  installed LaunchAgents and anything else that looks like a previous install;
  if it finds one, stop and ask.
- **Prefer dry runs.** Every apply step below has a dry run; show its output
  before asking for the real one.
- Do not commit, push or open pull requests unless the user asks.

## 2. Setup flow

1. Run the entry script for this OS; it is read-only apart from building:
   - macOS: `sh scripts/setup.sh`
   - Windows (PowerShell): `powershell -ExecutionPolicy Bypass -File scripts\setup.ps1`

   It checks the OS, the Rust toolchain and (on macOS) Karabiner-Elements and
   the Xcode command line tools, builds, and runs `kfm-config doctor`. If Rust
   is missing it says how to install it; ask before installing anything. On a
   fresh download it ends with exit code 1 and "config.sh does not exist yet":
   that is expected, continue with step 2. If it reports an existing
   installation, stop here and ask the user.
   Each computer is set up separately: run this agent (or the commands) on the
   source and on the receiver; an agent on one computer cannot configure the
   other.
2. Ask the user only what you cannot detect: which computer is the source
   (the one the keyboard is plugged into) and which is the receiver; the
   receiver's OS; where the receiver sits in their Logitech Flow layout (left,
   right, above, below; which display); the keyboard if it is not the tested
   one (vendor/product id as Karabiner-Elements shows them).
3. Create the settings: `kfm-config init --role <mac-source|receiver|windows-source>`,
   then `kfm-config set ...` for each value. On the receiver computer, repeat
   with `--role receiver`. Settings live in `./local/` (ignored by git):
   `local/config.sh` (addresses, layout, target OS) and `local/keymap.json`
   (hotkey and key remaps). These two files are the only place settings live;
   do not create others. Make the key on the source with the command `init`
   prints (`kfm-mac keygen --out <path>`); the user copies that file to the
   other computer and sets `KFM_KEY` there to where they put it.
4. `kfm-config validate`, then `kfm-config doctor`.
5. Dry runs: `kfm-config apply` prints the exact commands for this computer,
   each with its dry-run form and its rollback. Run the dry-run forms, show
   the output, then ask before each real step.

## 3. Changing settings later (the common request)

Map natural requests to `kfm-config` commands; always `--dry-run` first,
show the before → after preview, then write. A write validates everything,
keeps a backup in `local/backups/`, and refuses an invalid result (nothing is
written then).

| The user says | Run (add `--dry-run` first) |
|---|---|
| "Swap Caps Lock and Ctrl on Windows" | `kfm-config swap windows caps_lock left_control` |
| "Make Caps Lock a Ctrl on the Mac" | `kfm-config map mac caps_lock left_control` (refused if another key would still send Ctrl: then `swap`, or `drop` the other key) |
| "Turn off right Option on the Mac" | `kfm-config drop mac right_option` |
| "Undo that one key" | `kfm-config unmap mac right_option` |
| "Use F15 to switch" | `kfm-config hotkey f15` (F13–F20 only; the Karabiner rule must then be applied again) |
| "Put it back to default" | `kfm-config reset windows`, `reset mac` or `reset all` |
| "My Windows PC is now above the Mac" | on the Mac `kfm-config set KFM_FLOW_EDGE top`, on the receiver `set KFM_FLOW_EDGE bottom`; first check `kfm-config layout displays` on each computer to see which edges are outer edges |
| "Use only my second display" | `kfm-config set KFM_FLOW_DISPLAY 2` (`N[,M]` or `all`, numbers as `layout displays` prints them) |
| "Which keys can I change?" | `kfm-config keys` |

The target is the **receiver's** OS (`windows` or `mac`): the computer the
keys are sent to. `kfm-config` is `./target/release/kfm-config` (`.exe` on
Windows) after the setup script has built it. `kfm-config --help` lists the exact commands. Key names are
Karabiner-Elements `key_code` names (`kfm-config keys`); left and
right modifiers are separate keys.

After a change, `kfm-config status` says whether each file is edited,
validated, applied, or needs a restart, and whether the Karabiner rule must
be applied again. Applying is the same approved flow as in step 5.

Things to refuse or warn about: a hotkey other than F13–F20 (it would stop
working on the source Mac too); remapping the hotkey itself; two keys ending
up as the same key; removing every way back (the hotkey is the escape and is
always kept). `kfm-config` enforces these; explain the refusal in plain words.

## 4. When something goes wrong

- Keyboard stopped typing on the Mac: README "Recovery".
- Undo a settings change: `kfm-config restore --list`, then
  `kfm-config restore <timestamp>`.
- Undo an install: `sh scripts/resident/uninstall.sh` (dry run), then
  `--apply` with the user's yes; `kfm-karabiner restore` for the Karabiner
  rule (it keeps a backup).
- For each failure, tell the user the cause, the next action, and how to roll
  back.

## 5. What is not available

- No graphical settings editor; settings are edited through `kfm-config`
  (you can drive it from natural language as above).
- Per-keyboard capture on Windows, Windows → Windows, Linux, more than one
  receiver.

## For contributors

`cargo test`, `cargo clippy --all-targets -- -D warnings`, and for the
Windows target `cargo clippy --target x86_64-pc-windows-msvc --bins --lib -- -D warnings`.
Keep README.md, SPEC.md and this file in line with what is actually tested.
