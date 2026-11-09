#!/bin/sh
# Resident guard (launchd local.kfm-guard, every 10 s; independent of kfm-mac).
# 1. If kfm-mac is not running, force kfm_route to 0 once (Karabiner keeps
#    suppressing the keyboard while route 1, so a dead receiver must not leave it on).
# 2. Rotate the kfm-mac log by size (copy, then truncate: kfm-mac appends).
# Logs times only, never key contents.
APP="$HOME/Library/Application Support/keyboard-follows-mouse"
LOGS="$HOME/Library/Logs/keyboard-follows-mouse"
K="${KFM_KARABINER_CLI:-/Library/Application Support/org.pqrs/Karabiner-Elements/bin/karabiner_cli}"  # override only for tests
MARK="$APP/run/route0-set"
GL="$LOGS/kfm-guard.log"
MAX=5242880   # 5 MB
KEEP=5

if pgrep -f "^$APP/bin/kfm-mac run" >/dev/null; then
  rm -f "$MARK"
elif [ ! -e "$MARK" ]; then
  if "$K" --set-variables '{"kfm_route":0}'; then
    : > "$MARK"; echo "$(date '+%F %T') kfm-mac not running: route 0 set" >> "$GL"
  else
    echo "$(date '+%F %T') kfm-mac not running: setting route 0 FAILED" >> "$GL"
  fi
fi

for f in "$LOGS/kfm-mac.log" "$GL"; do
  [ -f "$f" ] || continue
  size=$(stat -f %z "$f")
  [ "$size" -gt "$MAX" ] || continue
  i=$KEEP
  while [ $i -gt 1 ]; do
    [ -f "$f.$((i-1))" ] && mv -f "$f.$((i-1))" "$f.$i"
    i=$((i-1))
  done
  cp "$f" "$f.1" && : > "$f"
done
exit 0
