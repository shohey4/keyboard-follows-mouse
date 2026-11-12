//! Normalized physical keys and the platform code tables.
//!
//! The router works on USB HID keyboard usages (page 0x07): they name a
//! physical key independent of OS and layout. macOS virtual keycodes are
//! converted in on capture; Windows set-1 scan codes are produced on output.

/// USB HID Keyboard/Keypad usage (page 0x07).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Usage(pub u16);

impl Usage {
    /// Left/right Ctrl, Shift, Alt(Option), GUI(Cmd/Win).
    pub fn is_modifier(self) -> bool {
        (0xE0..=0xE7).contains(&self.0)
    }
}

pub mod hid {
    use super::Usage;
    pub const A: Usage = Usage(0x04);
    pub const C: Usage = Usage(0x06);
    pub const V: Usage = Usage(0x19);
    pub const BACKSLASH: Usage = Usage(0x31);
    pub const F12: Usage = Usage(0x45);
    pub const LANG1: Usage = Usage(0x90); // かな / IME on
    pub const LANG2: Usage = Usage(0x91); // 英数 / IME off
    pub const LCTRL: Usage = Usage(0xE0);
    pub const LSHIFT: Usage = Usage(0xE1);
    pub const LALT: Usage = Usage(0xE2);
    pub const LGUI: Usage = Usage(0xE3);
    pub const RCTRL: Usage = Usage(0xE4);
    pub const RSHIFT: Usage = Usage(0xE5);
    pub const RALT: Usage = Usage(0xE6);
    pub const RGUI: Usage = Usage(0xE7);
}

/// macOS virtual keycode (kVK_*) -> HID usage. US ANSI plus the JIS IME keys.
const MAC_TO_HID: &[(u16, u16)] = &[
    (0x00, 0x04), (0x01, 0x16), (0x02, 0x07), (0x03, 0x09), (0x04, 0x0B), (0x05, 0x0A),
    (0x06, 0x1D), (0x07, 0x1B), (0x08, 0x06), (0x09, 0x19), (0x0B, 0x05), (0x0C, 0x14),
    (0x0D, 0x1A), (0x0E, 0x08), (0x0F, 0x15), (0x10, 0x1C), (0x11, 0x17), (0x12, 0x1E),
    (0x13, 0x1F), (0x14, 0x20), (0x15, 0x21), (0x16, 0x23), (0x17, 0x22), (0x18, 0x2E),
    (0x19, 0x26), (0x1A, 0x24), (0x1B, 0x2D), (0x1C, 0x25), (0x1D, 0x27), (0x1E, 0x30),
    (0x1F, 0x12), (0x20, 0x18), (0x21, 0x2F), (0x22, 0x0C), (0x23, 0x13), (0x24, 0x28),
    (0x25, 0x0F), (0x26, 0x0D), (0x27, 0x34), (0x28, 0x0E), (0x29, 0x33), (0x2A, 0x31),
    (0x2B, 0x36), (0x2C, 0x38), (0x2D, 0x11), (0x2E, 0x10), (0x2F, 0x37), (0x30, 0x2B),
    (0x31, 0x2C), (0x32, 0x35), (0x33, 0x2A), (0x35, 0x29), (0x36, 0xE7), (0x37, 0xE3),
    (0x38, 0xE1), (0x39, 0x39), (0x3A, 0xE2), (0x3B, 0xE0), (0x3C, 0xE5), (0x3D, 0xE6),
    (0x3E, 0xE4), (0x60, 0x3E), (0x61, 0x3F), (0x62, 0x40), (0x63, 0x3C), (0x64, 0x41),
    (0x65, 0x42), (0x66, 0x91), (0x67, 0x44), (0x68, 0x90), (0x6D, 0x43), (0x6F, 0x45),
    (0x72, 0x49), (0x73, 0x4A), (0x74, 0x4B), (0x75, 0x4C), (0x76, 0x3D), (0x77, 0x4D), (0x78, 0x3B),
    (0x79, 0x4E), (0x7A, 0x3A), (0x7B, 0x50), (0x7C, 0x4F), (0x7D, 0x51), (0x7E, 0x52),
];

pub fn usage_from_mac_keycode(keycode: u16) -> Option<Usage> {
    MAC_TO_HID.iter().find(|(k, _)| *k == keycode).map(|(_, u)| Usage(*u))
}

/// A Windows SendInput target: a set-1 scan code (KEYEVENTF_SCANCODE, plus
/// KEYEVENTF_EXTENDEDKEY when `extended`) or, for keys without a stable scan
/// code such as IME on/off, a virtual key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WinKey {
    Scan { code: u8, extended: bool },
    Vk(u8),
}

pub const VK_IME_ON: u8 = 0x16;
pub const VK_IME_OFF: u8 = 0x1A;

impl WinKey {
    /// 16-bit wire id: 0x0000|ext<<8|scan, or 0x8000|vk.
    pub fn wire(self) -> u16 {
        match self {
            WinKey::Scan { code, extended } => (u16::from(extended) << 8) | u16::from(code),
            WinKey::Vk(vk) => 0x8000 | u16::from(vk),
        }
    }
    pub fn from_wire(w: u16) -> Option<WinKey> {
        if w & 0x8000 != 0 {
            if w & 0x7F00 != 0 { return None; }
            Some(WinKey::Vk((w & 0xFF) as u8))
        } else {
            if w & 0xFE00 != 0 || w & 0xFF == 0 { return None; }
            Some(WinKey::Scan { code: (w & 0xFF) as u8, extended: w & 0x100 != 0 })
        }
    }
    /// Modifiers are released last so a lost key never leaves a chord half-applied.
    pub fn is_modifier(self) -> bool {
        matches!(self,
            WinKey::Scan { code: 0x1D, .. } | WinKey::Scan { code: 0x2A, extended: false }
            | WinKey::Scan { code: 0x36, extended: false } | WinKey::Scan { code: 0x38, .. }
            | WinKey::Scan { code: 0x5B, extended: true } | WinKey::Scan { code: 0x5C, extended: true })
    }
}

/// HID usage -> Windows set-1 scan code (extended flag = E0 prefix).
const HID_TO_WIN: &[(u16, u8, bool)] = &[
    (0x04, 0x1E, false), (0x05, 0x30, false), (0x06, 0x2E, false), (0x07, 0x20, false),
    (0x08, 0x12, false), (0x09, 0x21, false), (0x0A, 0x22, false), (0x0B, 0x23, false),
    (0x0C, 0x17, false), (0x0D, 0x24, false), (0x0E, 0x25, false), (0x0F, 0x26, false),
    (0x10, 0x32, false), (0x11, 0x31, false), (0x12, 0x18, false), (0x13, 0x19, false),
    (0x14, 0x10, false), (0x15, 0x13, false), (0x16, 0x1F, false), (0x17, 0x14, false),
    (0x18, 0x16, false), (0x19, 0x2F, false), (0x1A, 0x11, false), (0x1B, 0x2D, false),
    (0x1C, 0x15, false), (0x1D, 0x2C, false),
    (0x1E, 0x02, false), (0x1F, 0x03, false), (0x20, 0x04, false), (0x21, 0x05, false),
    (0x22, 0x06, false), (0x23, 0x07, false), (0x24, 0x08, false), (0x25, 0x09, false),
    (0x26, 0x0A, false), (0x27, 0x0B, false),
    (0x28, 0x1C, false), (0x29, 0x01, false), (0x2A, 0x0E, false), (0x2B, 0x0F, false),
    (0x2C, 0x39, false), (0x2D, 0x0C, false), (0x2E, 0x0D, false), (0x2F, 0x1A, false),
    (0x30, 0x1B, false), (0x31, 0x2B, false), (0x33, 0x27, false), (0x34, 0x28, false),
    (0x35, 0x29, false), (0x36, 0x33, false), (0x37, 0x34, false), (0x38, 0x35, false),
    (0x39, 0x3A, false),
    (0x3A, 0x3B, false), (0x3B, 0x3C, false), (0x3C, 0x3D, false), (0x3D, 0x3E, false),
    (0x3E, 0x3F, false), (0x3F, 0x40, false), (0x40, 0x41, false), (0x41, 0x42, false),
    (0x42, 0x43, false), (0x43, 0x44, false), (0x44, 0x57, false), (0x45, 0x58, false),
    (0x49, 0x52, true), (0x4A, 0x47, true), (0x4B, 0x49, true), (0x4C, 0x53, true),
    (0x4D, 0x4F, true), (0x4E, 0x51, true), (0x4F, 0x4D, true), (0x50, 0x4B, true),
    (0x51, 0x50, true), (0x52, 0x48, true),
    (0xE0, 0x1D, false), (0xE1, 0x2A, false), (0xE2, 0x38, false), (0xE3, 0x5B, true),
    (0xE4, 0x1D, true), (0xE5, 0x36, false), (0xE6, 0x38, true), (0xE7, 0x5C, true),
];

/// The usage a wire key stands for (inverse of `win_key_for_usage`). A receiver
/// that is not Windows uses it to turn the wire key back into a physical key.
pub fn usage_for_win_key(k: WinKey) -> Option<Usage> {
    match k {
        WinKey::Vk(VK_IME_ON) => Some(hid::LANG1),
        WinKey::Vk(VK_IME_OFF) => Some(hid::LANG2),
        WinKey::Vk(_) => None,
        WinKey::Scan { code, extended } => HID_TO_WIN.iter().find(|(_, c, e)| *c == code && *e == extended).map(|(h, ..)| Usage(*h)),
    }
}

/// HID usage -> macOS virtual keycode (inverse of `usage_from_mac_keycode`).
pub fn mac_keycode_for_usage(u: Usage) -> Option<u16> {
    MAC_TO_HID.iter().find(|(_, h)| *h == u.0).map(|(k, _)| *k)
}

pub fn win_key_for_usage(u: Usage) -> Option<WinKey> {
    if u == hid::LANG1 { return Some(WinKey::Vk(VK_IME_ON)); }
    if u == hid::LANG2 { return Some(WinKey::Vk(VK_IME_OFF)); }
    HID_TO_WIN.iter().find(|(h, _, _)| *h == u.0).map(|(_, code, extended)| WinKey::Scan { code: *code, extended: *extended })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_keycodes_are_unique_and_map_to_windows() {
        for (i, (k, u)) in MAC_TO_HID.iter().enumerate() {
            assert!(MAC_TO_HID[i + 1..].iter().all(|(k2, _)| k2 != k), "duplicate mac keycode {k:#x}");
            assert!(win_key_for_usage(Usage(*u)).is_some(), "mac {k:#x} -> hid {u:#x} has no Windows key");
        }
    }

    #[test]
    fn windows_codes_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for (h, ..) in HID_TO_WIN {
            assert!(seen.insert(win_key_for_usage(Usage(*h)).unwrap()), "two usages share a Windows key ({h:#x})");
        }
    }

    #[test]
    fn known_keys() {
        assert_eq!(usage_from_mac_keycode(0x08), Some(hid::C));
        assert_eq!(usage_from_mac_keycode(0x37), Some(hid::LGUI));
        assert_eq!(win_key_for_usage(hid::C), Some(WinKey::Scan { code: 0x2E, extended: false }));
        assert_eq!(win_key_for_usage(hid::RCTRL), Some(WinKey::Scan { code: 0x1D, extended: true }));
        assert_eq!(win_key_for_usage(hid::LANG1), Some(WinKey::Vk(VK_IME_ON)));
    }

    #[test]
    fn every_wire_key_maps_back_to_its_usage_and_to_a_mac_keycode() {
        for (h, ..) in HID_TO_WIN {
            let u = Usage(*h);
            assert_eq!(usage_for_win_key(win_key_for_usage(u).unwrap()), Some(u), "{h:#x}");
            assert!(mac_keycode_for_usage(u).is_some(), "hid {h:#x} has no macOS keycode");
        }
        for u in [hid::LANG1, hid::LANG2] { assert_eq!(usage_for_win_key(win_key_for_usage(u).unwrap()), Some(u)); }
        assert_eq!(usage_for_win_key(WinKey::Vk(0x41)), None, "unknown virtual keys are not guessed");
        assert_eq!(mac_keycode_for_usage(hid::C), Some(0x08));
    }

    #[test]
    fn wire_roundtrip_and_rejects_garbage() {
        for (h, ..) in HID_TO_WIN {
            let k = win_key_for_usage(Usage(*h)).unwrap();
            assert_eq!(WinKey::from_wire(k.wire()), Some(k));
        }
        assert_eq!(WinKey::from_wire(WinKey::Vk(VK_IME_OFF).wire()), Some(WinKey::Vk(VK_IME_OFF)));
        assert_eq!(WinKey::from_wire(0x0000), None);
        assert_eq!(WinKey::from_wire(0x0400 | 0x1E), None);
        assert_eq!(WinKey::from_wire(0x8100), None);
    }
}
