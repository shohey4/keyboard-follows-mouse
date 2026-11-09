//! physical key -> normalized action -> target-specific key.
//!
//! v0 only remaps for the Windows target; the Mac target is the untouched
//! local path. The mapping is resolved once at key-down and remembered by the
//! router, so a profile change while a key is held can never strand its key-up.

use std::collections::HashMap;

use crate::keys::{hid, win_key_for_usage, Usage, WinKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side { Left, Right }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Same physical key on the target.
    Key(Usage),
    /// Cmd on macOS, Ctrl on Windows.
    PrimaryModifier(Side),
    /// Swallowed for this target.
    Drop,
}

#[derive(Clone, Debug, Default)]
pub struct WindowsProfile {
    overrides: HashMap<Usage, Action>,
}

impl WindowsProfile {
    /// Every key is sent as the same physical key (Cmd arrives as Win).
    pub fn identity() -> Self { Self::default() }

    /// Mac muscle memory on Windows: Cmd acts as Ctrl, Ctrl reaches the Win key.
    pub fn mac_like() -> Self {
        let mut p = Self::default();
        p.set(hid::LGUI, Action::PrimaryModifier(Side::Left));
        p.set(hid::RGUI, Action::PrimaryModifier(Side::Right));
        p.set(hid::LCTRL, Action::Key(hid::LGUI));
        p.set(hid::RCTRL, Action::Key(hid::RGUI));
        p
    }

    /// Mode A: Karabiner reports keys after its simple modifications; send each
    /// listed (reported, physical) key to Windows as the physical key.
    pub fn undo_swaps(map: &[(Usage, Usage)]) -> Self {
        let mut p = Self::default();
        for &(reported, physical) in map { p.set(reported, Action::Key(physical)); }
        p
    }

    pub fn set(&mut self, physical: Usage, action: Action) { self.overrides.insert(physical, action); }

    pub fn action(&self, physical: Usage) -> Action {
        self.overrides.get(&physical).copied().unwrap_or(Action::Key(physical))
    }

    /// None = no Windows equivalent (dropped, never guessed).
    pub fn resolve(&self, physical: Usage) -> Option<WinKey> {
        match self.action(physical) {
            Action::Key(u) => win_key_for_usage(u),
            Action::PrimaryModifier(Side::Left) => win_key_for_usage(hid::LCTRL),
            Action::PrimaryModifier(Side::Right) => win_key_for_usage(hid::RCTRL),
            Action::Drop => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_like_moves_cmd_to_ctrl_and_keeps_win_reachable() {
        let p = WindowsProfile::mac_like();
        assert_eq!(p.resolve(hid::LGUI), win_key_for_usage(hid::LCTRL));
        assert_eq!(p.resolve(hid::RGUI), win_key_for_usage(hid::RCTRL));
        assert_eq!(p.resolve(hid::LCTRL), win_key_for_usage(hid::LGUI));
        assert_eq!(p.resolve(hid::C), win_key_for_usage(hid::C));
    }

    #[test]
    fn identity_and_drop() {
        let mut p = WindowsProfile::identity();
        assert_eq!(p.resolve(hid::LGUI), win_key_for_usage(hid::LGUI));
        p.set(hid::F12, Action::Drop);
        assert_eq!(p.resolve(hid::F12), None);
        assert_eq!(p.resolve(Usage(0x01)), None); // no Windows key: not guessed
    }
}
