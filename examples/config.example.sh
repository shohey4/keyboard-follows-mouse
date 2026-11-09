# Settings for scripts/resident/install.sh (pass them as environment variables).
# The installer writes the network and keyboard settings to
# ~/Library/Application Support/keyboard-follows-mouse/config.sh.
# Addresses below are documentation examples (RFC 5737); use your own.

KFM_KEY=~/kfm-psk.key               # made by `kfm-mac keygen --out ...`; copied, then installed 0600
KFM_PEER=192.0.2.10:47501           # the Windows PC running kfm-win --bind
KFM_BIND=192.0.2.20:47502           # this Mac's LAN address (kfm-win --peer)
KFM_FLOW_EDGE=right                 # the Mac screen edge that faces the Windows PC. Flow-follow needs
                                    # the Windows PC to the right (its left edge is watched), so: right
KFM_FLOW_DISPLAY=all                # which Mac display(s) that edge belongs to: all, or N[,M]
KFM_LINK_TIMEOUT=                   # ms without a verified Ack before the link is down (empty = 300)

# Only for a keyboard other than the tested one (both, as Karabiner-Elements shows them):
KFM_KEYBOARD_VENDOR_ID=
KFM_KEYBOARD_PRODUCT_ID=
