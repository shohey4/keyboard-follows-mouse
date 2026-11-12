//! Pure half of the macOS receiver's injector: wire key -> macOS virtual
//! keycode, and the modifier flags every posted event must carry (macOS apps
//! read Cmd/Ctrl/Opt/Shift from the event flags, not from earlier key events).
//! The binary posts the events with CGEventPost; nothing here touches the OS.

use std::collections::BTreeSet;

use crate::keys::{hid, mac_keycode_for_usage, usage_for_win_key, Usage, WinKey};

// CGEventFlags (device-independent) and the NX_DEVICE* bits for left/right.
const SHIFT: u64 = 0x0002_0000;
const CONTROL: u64 = 0x0004_0000;
const ALTERNATE: u64 = 0x0008_0000;
const COMMAND: u64 = 0x0010_0000;
const NON_COALESCED: u64 = 0x0000_0100;
const NUMERIC_PAD: u64 = 0x0020_0000;
const SECONDARY_FN: u64 = 0x0080_0000;

/// Flags real hardware events carry for the key itself: arrows have NumericPad
/// and SecondaryFn; F-keys, Home/End/Page/forward delete and Help have
/// SecondaryFn. Apps that compare modifier masks expect them.
fn key_bits(u: Usage) -> u64 {
    match u.0 {
        0x4F..=0x52 => NUMERIC_PAD | SECONDARY_FN,
        0x3A..=0x45 | 0x49..=0x4E => SECONDARY_FN,
        _ => 0,
    }
}

fn modifier_bits(u: Usage) -> Option<u64> {
    Some(match u {
        hid::LCTRL => CONTROL | 0x0000_0001,
        hid::LSHIFT => SHIFT | 0x0000_0002,
        hid::RSHIFT => SHIFT | 0x0000_0004,
        hid::LGUI => COMMAND | 0x0000_0008,
        hid::RGUI => COMMAND | 0x0000_0010,
        hid::LALT => ALTERNATE | 0x0000_0020,
        hid::RALT => ALTERNATE | 0x0000_0040,
        hid::RCTRL => CONTROL | 0x0000_2000,
        _ => return None,
    })
}

/// One event to post: keycode, key-down or key-up, whether it is a modifier
/// (posted as a flags-changed event), and the flags after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacEvent { pub keycode: u16, pub down: bool, pub modifier: bool, pub flags: u64 }

/// Tracks held modifiers so each event carries the right flags.
#[derive(Default)]
pub struct MacKeyboard { held_modifiers: BTreeSet<Usage> }

impl MacKeyboard {
    /// None = no macOS key for it (not guessed; the caller counts a drop).
    pub fn event(&mut self, key: WinKey, down: bool) -> Option<MacEvent> {
        let u = usage_for_win_key(key)?;
        let keycode = mac_keycode_for_usage(u)?;
        let modifier = modifier_bits(u).is_some();
        if modifier { if down { self.held_modifiers.insert(u); } else { self.held_modifiers.remove(&u); } }
        Some(MacEvent { keycode, down, modifier, flags: self.flags() | key_bits(u) })
    }

    pub fn flags(&self) -> u64 {
        self.held_modifiers.iter().filter_map(|u| modifier_bits(*u)).fold(NON_COALESCED, |a, b| a | b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::win_key_for_usage;

    fn k(u: Usage) -> WinKey { win_key_for_usage(u).unwrap() }

    #[test]
    fn cmd_c_carries_the_command_flag_and_releases_cleanly() {
        let mut m = MacKeyboard::default();
        let cmd = m.event(k(hid::LGUI), true).unwrap();
        assert_eq!((cmd.keycode, cmd.modifier), (0x37, true));
        assert_ne!(cmd.flags & COMMAND, 0);
        let c = m.event(k(hid::C), true).unwrap();
        assert_eq!((c.keycode, c.modifier), (0x08, false));
        assert_ne!(c.flags & COMMAND, 0, "C is Cmd+C");
        m.event(k(hid::C), false).unwrap();
        let up = m.event(k(hid::LGUI), false).unwrap();
        assert_eq!(up.flags & COMMAND, 0);
        assert_eq!(m.flags(), NON_COALESCED);
    }

    #[test]
    fn left_and_right_modifiers_are_tracked_separately() {
        let mut m = MacKeyboard::default();
        m.event(k(hid::LSHIFT), true);
        m.event(k(hid::RSHIFT), true);
        let e = m.event(k(hid::LSHIFT), false).unwrap();
        assert_ne!(e.flags & SHIFT, 0, "right Shift is still down");
        assert_eq!(e.flags & 0x2, 0);
        assert_ne!(e.flags & 0x4, 0);
    }

    #[test]
    fn arrows_and_function_keys_carry_their_own_flags() {
        let mut m = MacKeyboard::default();
        m.event(k(hid::LSHIFT), true);
        let left = m.event(k(Usage(0x50)), true).unwrap();
        assert_eq!(left.keycode, 0x7B);
        assert_eq!(left.flags & (NUMERIC_PAD | SECONDARY_FN | SHIFT), NUMERIC_PAD | SECONDARY_FN | SHIFT, "Shift+Left");
        assert_eq!(m.event(k(hid::F12), true).unwrap().flags & (SECONDARY_FN | NUMERIC_PAD), SECONDARY_FN);
        assert_eq!(m.event(k(hid::C), true).unwrap().flags & (SECONDARY_FN | NUMERIC_PAD), 0);
    }

    #[test]
    fn ime_keys_map_and_unknown_keys_are_not_guessed() {
        let mut m = MacKeyboard::default();
        assert_eq!(m.event(k(hid::LANG1), true).unwrap().keycode, 0x68, "kVK_JIS_Kana");
        assert_eq!(m.event(k(hid::LANG2), true).unwrap().keycode, 0x66, "kVK_JIS_Eisu");
        assert_eq!(m.event(WinKey::Vk(0x41), true), None);
        assert_eq!(m.event(WinKey::Scan { code: 0x7F, extended: false }, true), None);
    }
}
