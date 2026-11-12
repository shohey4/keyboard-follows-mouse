#!/bin/sh
# Install the resident Mac side: two LaunchAgents for this user.
#   local.kfm-guard  every 10 s: route 0 whenever kfm-mac is not running; log rotation
#   local.kfm-mac    kfm-mac at login, restarted after a crash or K6 (not after a clean stop)
# DRY RUN by default (prints every step, changes nothing). --apply does it.
#   KFM_KEY=<psk file> KFM_PEER=<windows-ip>:47501 KFM_BIND=<this-mac-ip>:47502
#   [KFM_FLOW_EDGE=right] [KFM_FLOW_DISPLAY=all] [KFM_LINK_TIMEOUT=ms]
#   [KFM_KEYBOARD_VENDOR_ID=0x.... KFM_KEYBOARD_PRODUCT_ID=0x....]
#   [KFM_TARGET_OS=windows|mac] [KFM_KEYMAP=<keymap.json>] sh install.sh [--apply]
# Never changes Karabiner's configuration (its kfm rule must already be installed).
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP="$HOME/Library/Application Support/keyboard-follows-mouse"
LOGS="$HOME/Library/Logs/keyboard-follows-mouse"
LA="$HOME/Library/LaunchAgents"
DOMAIN="gui/$(id -u)"
SOCK="/Library/Application Support/org.pqrs/tmp/user/$(id -u)/user_command_receiver.sock"
APPLY=0; [ "${1:-}" = --apply ] && APPLY=1
run() { echo "  + $*"; [ $APPLY = 1 ] && { "$@" || { echo "FAILED: $*"; exit 1; }; }; return 0; }

: "${KFM_KEY:?set KFM_KEY to the pre-shared key file}"
PEER="${KFM_PEER:?set KFM_PEER to the Windows receiver, e.g. 192.0.2.10:47501}"; BIND="${KFM_BIND:?set KFM_BIND to this Mac LAN address, e.g. 192.0.2.20:47502}"
EDGE="${KFM_FLOW_EDGE:-right}"; DISP="${KFM_FLOW_DISPLAY:-all}"; LT="${KFM_LINK_TIMEOUT:-}"
KV="${KFM_KEYBOARD_VENDOR_ID:-}"; KP="${KFM_KEYBOARD_PRODUCT_ID:-}"
TOS="${KFM_TARGET_OS:-windows}"; KM="${KFM_KEYMAP:-}"
# The kfm-karabiner calls below must see the same keyboard the rule will capture: pass both ids on, or unset
# both when empty (an exported empty id, e.g. from `set -a; . local/config.sh`, would make kfm-karabiner refuse).
if [ -n "$KV$KP" ]; then export KFM_KEYBOARD_VENDOR_ID="$KV" KFM_KEYBOARD_PRODUCT_ID="$KP"; else unset KFM_KEYBOARD_VENDOR_ID KFM_KEYBOARD_PRODUCT_ID; fi
case "$TOS" in windows|mac) ;; *) echo "KFM_TARGET_OS must be windows or mac"; exit 1;; esac

echo "checks:"
[ -x "$ROOT/target/release/kfm-mac" ] || { echo "  build first: (cd $ROOT && cargo build --release)"; exit 1; }
[ -r "$KFM_KEY" ] || { echo "  key not readable: $KFM_KEY"; exit 1; }
[ -z "$KM" ] || KFM_KEYMAP="$KM" "$ROOT/target/release/kfm-karabiner" rule >/dev/null || { echo "  keymap rejected: $KM"; exit 1; }
pgrep -x kfm-mac >/dev/null && { echo "  a kfm-mac is running; stop it first"; exit 1; }
for j in local.kfm-guard local.kfm-mac; do
  launchctl print "$DOMAIN/$j" >/dev/null 2>&1 && { echo "  $j is already loaded; run scripts/resident/uninstall.sh --apply first"; exit 1; }
done
[ -e "$SOCK" ] && { echo "  the receiver socket exists ($SOCK); stop whatever owns it first"; exit 1; }
KFM_KEYMAP="$KM" "$ROOT/target/release/kfm-karabiner" preflight | grep -q '^problems: \[\]' || { echo "  Karabiner preflight reports problems (run kfm-karabiner preflight)"; exit 1; }
KFM_KEYMAP="$KM" "$ROOT/target/release/kfm-karabiner" preflight | grep -q '(ours at Some(0))' || { echo "  the kfm rule is not installed first in the selected profile (run kfm-karabiner apply)"; exit 1; }
echo "  ok (binary, key, no receiver running, Karabiner rule present)"

echo "$( [ $APPLY = 1 ] && echo applying || echo 'DRY RUN (nothing changes; add --apply)'):"
run mkdir -p "$APP/bin" "$APP/key" "$APP/run" "$LOGS" "$LA"
run chmod 700 "$APP" "$APP/key" "$APP/run" "$LOGS"
run cp "$ROOT/target/release/kfm-mac" "$APP/bin/kfm-mac"
run cp "$ROOT/scripts/resident/kfm-mac-run.sh" "$ROOT/scripts/resident/kfm-guard.sh" "$ROOT/scripts/resident/kfmctl.sh" "$APP/bin/"
run cp "$KFM_KEY" "$APP/key/kfm-psk.key"
run chmod 600 "$APP/key/kfm-psk.key"
# The keymap must be the one the Karabiner rule was applied with (preflight above compares the hotkey).
if [ -n "$KM" ]; then run cp "$KM" "$APP/keymap.json"; else run rm -f "$APP/keymap.json"; fi
echo "  + write $APP/config.sh (peer $PEER, bind $BIND, edge $EDGE, display $DISP${LT:+, link timeout $LT ms})"
[ $APPLY = 1 ] && printf 'KFM_PEER=%s\nKFM_BIND=%s\nKFM_FLOW_EDGE=%s\nKFM_FLOW_DISPLAY=%s\nKFM_LINK_TIMEOUT=%s\nKFM_KEYBOARD_VENDOR_ID=%s\nKFM_KEYBOARD_PRODUCT_ID=%s\nKFM_TARGET_OS=%s\n' "$PEER" "$BIND" "$EDGE" "$DISP" "$LT" "$KV" "$KP" "$TOS" > "$APP/config.sh"
for j in local.kfm-guard local.kfm-mac; do
  echo "  + write $LA/$j.plist (from scripts/resident/$j.plist, HOME filled in)"
  if [ $APPLY = 1 ]; then
    sed "s|@HOME@|$HOME|g" "$ROOT/scripts/resident/$j.plist" > "$LA/$j.plist" && plutil -lint "$LA/$j.plist" >/dev/null || { echo "FAILED: $LA/$j.plist"; exit 1; }
  fi
done
# Guard first, so a receiver never runs without it.
run launchctl bootstrap "$DOMAIN" "$LA/local.kfm-guard.plist"
run launchctl bootstrap "$DOMAIN" "$LA/local.kfm-mac.plist"
echo "done. status: sh \"$APP/bin/kfmctl.sh\" status; log: $LOGS/kfm-mac.log; remove: sh $ROOT/scripts/resident/uninstall.sh --apply"
