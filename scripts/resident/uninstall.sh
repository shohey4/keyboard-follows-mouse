#!/bin/sh
# Remove the resident Mac side. DRY RUN by default; --apply does it.
# Order: stop kfm-mac (SIGTERM: it sets route 0 and releases Windows keys),
# stop the guard, set route 0 once more, remove the plists. The installed
# files (binary, key, logs) stay unless --purge is also given.
set -u
APP="$HOME/Library/Application Support/keyboard-follows-mouse"
LA="$HOME/Library/LaunchAgents"
DOMAIN="gui/$(id -u)"
K='/Library/Application Support/org.pqrs/Karabiner-Elements/bin/karabiner_cli'
APPLY=0; PURGE=0
for a in "$@"; do case "$a" in --apply) APPLY=1;; --purge) PURGE=1;; esac; done
run() { echo "  + $*"; [ $APPLY = 1 ] && "$@"; return 0; }
echo "$( [ $APPLY = 1 ] && echo removing || echo 'DRY RUN (nothing changes; add --apply)'):"
run launchctl bootout "$DOMAIN/local.kfm-mac"
run launchctl bootout "$DOMAIN/local.kfm-guard"
run "$K" --set-variables '{"kfm_route":0}'
run rm -f "$LA/local.kfm-mac.plist" "$LA/local.kfm-guard.plist"
[ $PURGE = 1 ] && run rm -rf "$APP"
echo "done. logs kept in $HOME/Library/Logs/keyboard-follows-mouse"
