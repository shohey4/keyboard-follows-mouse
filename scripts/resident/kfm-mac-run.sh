#!/bin/sh
# Resident launcher (launchd local.kfm-mac). Appends to the log (so the guard
# can rotate it by copy+truncate) and execs kfm-mac, so launchd sees kfm-mac
# itself: SIGTERM from `launchctl bootout` reaches it and it sets route 0.
APP="$HOME/Library/Application Support/keyboard-follows-mouse"
LOGS="$HOME/Library/Logs/keyboard-follows-mouse"
. "$APP/config.sh" || exit 78   # KFM_PEER KFM_BIND KFM_FLOW_EDGE KFM_FLOW_DISPLAY [KFM_LINK_TIMEOUT] [KFM_KEYBOARD_*]
# An empty keyboard id means "the tested default": unset it so kfm-mac does not see an empty value.
[ -n "${KFM_KEYBOARD_VENDOR_ID:-}" ] && export KFM_KEYBOARD_VENDOR_ID || unset KFM_KEYBOARD_VENDOR_ID
[ -n "${KFM_KEYBOARD_PRODUCT_ID:-}" ] && export KFM_KEYBOARD_PRODUCT_ID || unset KFM_KEYBOARD_PRODUCT_ID
# The installed keymap (hotkey and remaps), if any; kfm-mac refuses to start on an invalid one.
[ -f "$APP/keymap.json" ] && export KFM_KEYMAP="$APP/keymap.json" || unset KFM_KEYMAP
SOCK="/Library/Application Support/org.pqrs/tmp/user/$(id -u)/user_command_receiver.sock"
mkdir -p "$LOGS" && chmod 700 "$LOGS"
echo "=== start $(date '+%F %T') ===" >> "$LOGS/kfm-mac.log"
set -- run --peer "$KFM_PEER" --bind "$KFM_BIND" --key-file "$APP/key/kfm-psk.key" --enable-send \
  --socket-path "$SOCK" --use-karabiner-endpoint --apply-route \
  --flow-follow --flow-edge "$KFM_FLOW_EDGE" --flow-display "$KFM_FLOW_DISPLAY" \
  --resident --pause-file "$APP/paused" --target-os "${KFM_TARGET_OS:-windows}"
[ -n "${KFM_LINK_TIMEOUT:-}" ] && set -- "$@" --link-timeout-ms "$KFM_LINK_TIMEOUT"
exec "$APP/bin/kfm-mac" "$@" >> "$LOGS/kfm-mac.log" 2>&1
