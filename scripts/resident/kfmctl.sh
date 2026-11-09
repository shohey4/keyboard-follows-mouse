#!/bin/sh
# kfmctl pause|resume|status  (resident kfm-mac; nothing here changes Karabiner's config)
APP="$HOME/Library/Application Support/keyboard-follows-mouse"
case "$1" in
  pause)  : > "$APP/paused"; echo "paused: keys stay on the Mac within 1 s (kfm-mac keeps running)";;
  resume) rm -f "$APP/paused"; echo "resumed: the next Flow decision or Fn+F12 moves to Windows again";;
  status)
    [ -e "$APP/paused" ] && echo "pause file: present" || echo "pause file: absent"
    for j in local.kfm-mac local.kfm-guard; do
      if launchctl print "gui/$(id -u)/$j" >/dev/null 2>&1; then echo "$j: loaded"; else echo "$j: not loaded"; fi
    done
    pgrep -fl "^$APP/bin/kfm-mac run" >/dev/null && echo "kfm-mac: running" || echo "kfm-mac: not running"
    tail -n 3 "$HOME/Library/Logs/keyboard-follows-mouse/kfm-mac.log" 2>/dev/null;;
  *) echo "usage: kfmctl pause|resume|status"; exit 2;;
esac
