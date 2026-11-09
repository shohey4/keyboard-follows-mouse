//! Mac-side routing state machine (pure: no OS calls, no clock).
//!
//! The capture layer feeds every Rainy75 key event in (and only Rainy75's:
//! other keyboards never reach the router) and does exactly what
//! `KeyResult::local` says with the original event; `effects` go to the
//! sender and the menubar. Invariants:
//! - A key-up always goes where its key-down went (route is fixed at key-down),
//!   so switching while keys are held never strands a key on either side.
//! - Keys go to Windows only while Windows is selected, the link is up
//!   (= an authenticated Ack arrived recently) and macOS Secure Input is off.
//!   Selecting Windows while the link is down is refused.
//! - Losing the link while Windows is selected *holds* input by default:
//!   keys are swallowed, not re-routed to whatever app has focus on the Mac
//!   (text meant for Windows, e.g. a password, must not land there). The
//!   hotkey returns to the Mac in one press; the link coming back resumes
//!   Windows. `LinkLossPolicy::FallbackToMac` is the opt-in alternative.
//! - A key-up we never saw go down (pressed before start) passes to the Mac.

use std::collections::HashMap;

use crate::keymap::WindowsProfile;
use crate::keys::{Usage, WinKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target { Mac, Windows }

/// Where new key-downs go right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dest { Mac, Windows, Held }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason { Hotkey, Manual, Flow, EasySwitch, LinkDown, LinkUp, SecureInput }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LinkLossPolicy {
    /// Swallow keys until the link returns or the user switches to the Mac.
    #[default]
    Hold,
    /// Re-route to the Mac at once (risk: Windows-bound text reaches the Mac).
    FallbackToMac,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Local { Pass, Suppress }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Remote { key: WinKey, down: bool, repeat: bool },
    /// The effective destination changed (menubar / notification).
    DestChanged { dest: Dest, reason: Reason },
    /// A switch to Windows was refused because the link is not confirmed.
    RefusedLinkDown { reason: Reason },
}

#[derive(Debug, PartialEq, Eq)]
pub struct KeyResult { pub local: Local, pub effects: Vec<Effect> }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Local,
    Remote(WinKey),
    /// Swallowed (switch hotkey, held input, or no Windows equivalent).
    Consumed,
    /// Was remote, but the remote side has already been released.
    Lost,
}

/// Manual switch: `key` pressed while every `modifiers` key is held.
#[derive(Clone, Debug)]
pub struct Hotkey { pub key: Usage, pub modifiers: Vec<Usage> }

impl Default for Hotkey {
    /// F13 alone: a chord would leak its modifiers to the old target, and a
    /// lone Win/Alt press-release opens Start / the menu bar on Windows.
    fn default() -> Self { Hotkey { key: Usage(0x68), modifiers: vec![] } }
}

pub struct Router {
    selected: Target,
    link_up: bool,
    secure_input: bool,
    policy: LinkLossPolicy,
    held: HashMap<Usage, Route>,
    profile: WindowsProfile,
    hotkey: Hotkey,
    /// Key presses swallowed because the link was down while Windows was selected.
    pub held_drops: u64,
}

impl Router {
    pub fn new(profile: WindowsProfile, hotkey: Hotkey) -> Self {
        Router { selected: Target::Mac, link_up: false, secure_input: false, policy: LinkLossPolicy::default(), held: HashMap::new(), profile, hotkey, held_drops: 0 }
    }

    pub fn with_policy(mut self, policy: LinkLossPolicy) -> Self { self.policy = policy; self }

    pub fn effective(&self) -> Dest {
        match self.selected {
            Target::Mac => Dest::Mac,
            // Secure Input blinds the tap: keys reach the Mac whatever we decide.
            Target::Windows if self.secure_input => Dest::Mac,
            Target::Windows if self.link_up => Dest::Windows,
            Target::Windows => Dest::Held,
        }
    }

    pub fn on_key(&mut self, key: Usage, down: bool) -> KeyResult {
        if !down {
            return match self.held.remove(&key) {
                None | Some(Route::Local) => KeyResult { local: Local::Pass, effects: vec![] },
                Some(Route::Remote(k)) => suppress(vec![Effect::Remote { key: k, down: false, repeat: false }]),
                Some(Route::Consumed | Route::Lost) => suppress(vec![]),
            };
        }
        if let Some(route) = self.held.get(&key) {
            // Auto-repeat (or a duplicate down) follows the original route.
            return match *route {
                Route::Local => KeyResult { local: Local::Pass, effects: vec![] },
                Route::Remote(k) => suppress(vec![Effect::Remote { key: k, down: true, repeat: true }]),
                Route::Consumed | Route::Lost => suppress(vec![]),
            };
        }
        if key == self.hotkey.key && self.hotkey.modifiers.iter().all(|m| self.held.contains_key(m)) {
            self.held.insert(key, Route::Consumed);
            let next = if self.selected == Target::Mac { Target::Windows } else { Target::Mac };
            return suppress(self.select(next, Reason::Hotkey));
        }
        match self.effective() {
            Dest::Mac => {
                self.held.insert(key, Route::Local);
                KeyResult { local: Local::Pass, effects: vec![] }
            }
            Dest::Held => {
                self.held_drops += 1;
                self.held.insert(key, Route::Consumed);
                suppress(vec![])
            }
            Dest::Windows => match self.profile.resolve(key) {
                Some(k) => {
                    self.held.insert(key, Route::Remote(k));
                    suppress(vec![Effect::Remote { key: k, down: true, repeat: false }])
                }
                None => {
                    self.held.insert(key, Route::Consumed);
                    suppress(vec![])
                }
            },
        }
    }

    /// Hotkey / menubar / Flow follower selection. Held keys keep their routes.
    pub fn select(&mut self, target: Target, reason: Reason) -> Vec<Effect> {
        if target == Target::Windows && !self.link_up { return vec![Effect::RefusedLinkDown { reason }]; }
        self.transition(reason, |r| r.selected = target)
    }

    pub fn on_link(&mut self, up: bool) -> Vec<Effect> {
        if up { return self.transition(Reason::LinkUp, |r| r.link_up = true); }
        // "Down" is only our view (no Ack lately); the receiver may still hear
        // us when just the return path is lost. So release remote keys on a
        // best-effort basis too, and let the receiver's watchdog cover the
        // case where nothing gets through. Later key-ups are swallowed.
        let mut out = self.release_remote();
        let fallback = self.policy == LinkLossPolicy::FallbackToMac;
        out.extend(self.transition(Reason::LinkDown, |r| {
            r.link_up = false;
            if fallback { r.selected = Target::Mac; }
        }));
        out
    }

    /// macOS Secure Input hides key events from event taps, so key-ups for
    /// keys held on Windows would never arrive: release them now.
    pub fn on_secure_input(&mut self, active: bool) -> Vec<Effect> {
        let mut out = if active { self.release_remote() } else { vec![] };
        out.extend(self.transition(Reason::SecureInput, |r| r.secure_input = active));
        out
    }

    /// After the event tap was disabled/re-enabled (timeout, sleep), drop every
    /// key that is no longer physically down, releasing it remotely.
    pub fn resync(&mut self, physically_down: &[Usage]) -> Vec<Effect> {
        let stale: Vec<Usage> = self.held.keys().copied().filter(|k| !physically_down.contains(k)).collect();
        let mut ups = Vec::new();
        for k in stale {
            if let Some(Route::Remote(w)) = self.held.remove(&k) { ups.push(w); }
        }
        release_order(ups)
    }

    /// Release every key held on Windows now; their later key-ups are swallowed.
    /// Used when the capture layer stops seeing those keys (Secure Input, or
    /// mode A handing the keyboard back to the Mac).
    pub fn release_remote(&mut self) -> Vec<Effect> {
        let mut ups = Vec::new();
        for route in self.held.values_mut() {
            if let Route::Remote(w) = *route { ups.push(w); *route = Route::Lost; }
        }
        release_order(ups)
    }

    fn transition(&mut self, reason: Reason, f: impl FnOnce(&mut Self)) -> Vec<Effect> {
        let before = self.effective();
        f(self);
        let after = self.effective();
        if before == after { vec![] } else { vec![Effect::DestChanged { dest: after, reason }] }
    }
}

fn suppress(effects: Vec<Effect>) -> KeyResult { KeyResult { local: Local::Suppress, effects } }

/// Non-modifiers first, so a half-released chord never fires a shortcut.
fn release_order(mut keys: Vec<WinKey>) -> Vec<Effect> {
    keys.sort_by_key(|k| (k.is_modifier(), *k));
    keys.into_iter().map(|k| Effect::Remote { key: k, down: false, repeat: false }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{hid, win_key_for_usage};

    fn w(u: Usage) -> WinKey { win_key_for_usage(u).unwrap() }
    fn remote(u: Usage, down: bool) -> Effect { Effect::Remote { key: w(u), down, repeat: false } }
    fn changed(dest: Dest, reason: Reason) -> Effect { Effect::DestChanged { dest, reason } }
    fn f13() -> Usage { Hotkey::default().key }
    fn pass() -> KeyResult { KeyResult { local: Local::Pass, effects: vec![] } }

    fn on_windows() -> Router {
        let mut r = Router::new(WindowsProfile::identity(), Hotkey::default());
        r.on_link(true);
        r.select(Target::Windows, Reason::Manual);
        r
    }

    #[test]
    fn starts_on_mac_and_passes_through() {
        let mut r = Router::new(WindowsProfile::identity(), Hotkey::default());
        assert_eq!(r.effective(), Dest::Mac);
        assert_eq!(r.on_key(hid::A, true), pass());
        assert_eq!(r.on_key(hid::A, false).local, Local::Pass);
    }

    #[test]
    fn windows_target_suppresses_and_forwards() {
        let mut r = on_windows();
        assert_eq!(r.on_key(hid::A, true), suppress(vec![remote(hid::A, true)]));
        assert_eq!(r.on_key(hid::A, true).effects, vec![Effect::Remote { key: w(hid::A), down: true, repeat: true }]);
        assert_eq!(r.on_key(hid::A, false), suppress(vec![remote(hid::A, false)]));
    }

    #[test]
    fn hotkey_toggles_and_is_never_delivered() {
        let mut r = Router::new(WindowsProfile::identity(), Hotkey::default());
        r.on_link(true);
        assert_eq!(r.on_key(f13(), true), suppress(vec![changed(Dest::Windows, Reason::Hotkey)]));
        assert_eq!(r.on_key(f13(), false), suppress(vec![])); // its key-up goes nowhere either
        r.on_key(f13(), true);
        r.on_key(f13(), false);
        assert_eq!(r.effective(), Dest::Mac);
    }

    #[test]
    fn chord_hotkey_requires_modifiers() {
        let mut r = Router::new(WindowsProfile::identity(), Hotkey { key: hid::BACKSLASH, modifiers: vec![hid::RALT] });
        r.on_link(true);
        assert_eq!(r.on_key(hid::BACKSLASH, true).local, Local::Pass);
        r.on_key(hid::BACKSLASH, false);
        r.on_key(hid::RALT, true);
        assert_eq!(r.on_key(hid::BACKSLASH, true).local, Local::Suppress);
        assert_eq!(r.effective(), Dest::Windows);
    }

    #[test]
    fn windows_is_refused_without_a_confirmed_link() {
        let mut r = Router::new(WindowsProfile::identity(), Hotkey::default());
        assert_eq!(r.select(Target::Windows, Reason::Manual), vec![Effect::RefusedLinkDown { reason: Reason::Manual }]);
        assert_eq!(r.on_key(f13(), true), suppress(vec![Effect::RefusedLinkDown { reason: Reason::Hotkey }]));
        r.on_key(f13(), false);
        assert_eq!(r.effective(), Dest::Mac);
        assert_eq!(r.on_key(hid::A, true), pass()); // still typing on the Mac
    }

    #[test]
    fn key_up_follows_key_down_across_a_switch() {
        let mut r = on_windows();
        r.on_key(hid::LSHIFT, true); // held on Windows
        r.select(Target::Mac, Reason::Manual);
        assert_eq!(r.on_key(hid::A, true).local, Local::Pass); // new key -> Mac
        assert_eq!(r.on_key(hid::LSHIFT, false), suppress(vec![remote(hid::LSHIFT, false)]));

        r.on_key(hid::LALT, true); // held on Mac
        r.select(Target::Windows, Reason::Manual);
        assert_eq!(r.on_key(hid::LALT, false), pass());
    }

    #[test]
    fn link_loss_holds_input_instead_of_typing_into_the_mac() {
        let mut r = on_windows();
        r.on_key(hid::LGUI, true);
        assert_eq!(r.on_link(false), vec![remote(hid::LGUI, false), changed(Dest::Held, Reason::LinkDown)]);
        assert_eq!(r.held_drops, 0);
        r.on_key(hid::C, true);
        r.on_key(hid::C, false);
        assert_eq!(r.held_drops, 1, "a press swallowed while Held is counted once");
        assert_eq!(r.on_key(hid::LGUI, false), suppress(vec![])); // orphan up swallowed
        for k in [hid::A, hid::C, hid::V] {
            assert_eq!(r.on_key(k, true), suppress(vec![]), "held input must not reach the Mac");
            assert_eq!(r.on_key(k, false), suppress(vec![]));
        }
        // Link back: resumes the selected target (Windows).
        assert_eq!(r.on_link(true), vec![changed(Dest::Windows, Reason::LinkUp)]);
        assert_eq!(r.on_key(hid::A, true).effects, vec![remote(hid::A, true)]);
    }

    #[test]
    fn hotkey_is_the_one_press_escape_from_hold() {
        let mut r = on_windows();
        r.on_link(false);
        assert_eq!(r.on_key(f13(), true), suppress(vec![changed(Dest::Mac, Reason::Hotkey)]));
        r.on_key(f13(), false);
        assert_eq!(r.on_key(hid::A, true), pass());
        assert!(r.on_link(true).is_empty()); // stays on the Mac it was sent to
    }

    #[test]
    fn opt_in_fallback_policy_goes_to_the_mac() {
        let mut r = Router::new(WindowsProfile::identity(), Hotkey::default()).with_policy(LinkLossPolicy::FallbackToMac);
        r.on_link(true);
        r.select(Target::Windows, Reason::Manual);
        r.on_key(hid::LGUI, true);
        assert_eq!(r.on_link(false), vec![remote(hid::LGUI, false), changed(Dest::Mac, Reason::LinkDown)]);
        assert_eq!(r.on_key(hid::LGUI, false), suppress(vec![]));
        assert_eq!(r.on_key(hid::A, true), pass());
        assert!(r.on_link(true).is_empty());
    }

    #[test]
    fn secure_input_releases_remote_keys_modifiers_last() {
        let mut r = on_windows();
        r.on_key(hid::LCTRL, true);
        r.on_key(hid::C, true);
        let out = r.on_secure_input(true);
        assert_eq!(out, vec![remote(hid::C, false), remote(hid::LCTRL, false), changed(Dest::Mac, Reason::SecureInput)]);
        assert_eq!(r.on_key(hid::C, false), suppress(vec![]));
        assert_eq!(r.on_key(hid::V, true).local, Local::Pass);
        r.on_key(hid::V, false);
        assert_eq!(r.on_secure_input(false), vec![changed(Dest::Windows, Reason::SecureInput)]);
    }

    #[test]
    fn resync_releases_keys_no_longer_down() {
        let mut r = on_windows();
        r.on_key(hid::LSHIFT, true);
        r.on_key(hid::A, true);
        assert_eq!(r.resync(&[hid::LSHIFT]), vec![remote(hid::A, false)]);
        assert_eq!(r.on_key(hid::LSHIFT, false), suppress(vec![remote(hid::LSHIFT, false)]));
    }

    #[test]
    fn mapping_fixed_at_key_down() {
        let mut r = Router::new(WindowsProfile::mac_like(), Hotkey::default());
        r.on_link(true);
        r.select(Target::Windows, Reason::Manual);
        assert_eq!(r.on_key(hid::LGUI, true).effects, vec![remote(hid::LCTRL, true)]);
        assert_eq!(r.on_key(hid::LGUI, false).effects, vec![remote(hid::LCTRL, false)]);
    }

    #[test]
    fn unmapped_key_is_swallowed_on_windows() {
        let mut r = on_windows();
        assert_eq!(r.on_key(Usage(0x01), true), suppress(vec![]));
        assert_eq!(r.on_key(Usage(0x01), false), suppress(vec![]));
    }

    #[test]
    fn up_without_down_passes_to_mac() {
        let mut r = on_windows();
        assert_eq!(r.on_key(hid::A, false).local, Local::Pass);
    }

    /// No key event is ever delivered to both hosts, and every key-down that
    /// reached a host gets exactly one key-up on that same host.
    #[test]
    fn random_sequences_never_duplicate_or_strand() {
        let keys = [hid::A, hid::C, hid::LSHIFT, hid::LGUI, f13()];
        let mut rng: u64 = 0x2545F4914F6CDD1D;
        let mut next = || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };
        for _ in 0..300 {
            let mut r = Router::new(WindowsProfile::identity(), Hotkey::default());
            let (mut mac, mut win) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
            let mut phys = std::collections::BTreeSet::new();
            for _ in 0..80 {
                match next() % 10 {
                    0 | 1 => {
                        let effects = if next() % 2 == 0 { r.on_link(next() % 2 == 0) }
                            else { r.select(if next() % 2 == 0 { Target::Mac } else { Target::Windows }, Reason::Manual) };
                        for e in effects { if let Effect::Remote { key, down: false, .. } = e { assert!(win.remove(&key)); } }
                    }
                    _ => {
                        let k = keys[(next() % keys.len() as u64) as usize];
                        let down = !phys.contains(&k);
                        if down { phys.insert(k); } else { phys.remove(&k); }
                        let res = r.on_key(k, down);
                        let remote_hits: Vec<_> = res.effects.iter().filter(|e| matches!(e, Effect::Remote { .. })).collect();
                        assert!(res.local != Local::Pass || remote_hits.is_empty(), "delivered to both hosts");
                        if res.local == Local::Pass {
                            if down { assert!(mac.insert(k)); } else { mac.remove(&k); }
                        }
                        for e in remote_hits {
                            if let Effect::Remote { key, down: d, .. } = *e {
                                if d { assert!(win.insert(key)); } else { assert!(win.remove(&key), "remote up without down"); }
                            }
                        }
                    }
                }
            }
            // Release everything physically held: nothing may stay down anywhere
            // except keys the receiver's watchdog owns (routes lost to a link drop).
            for k in phys.clone() {
                let res = r.on_key(k, false);
                if res.local == Local::Pass { mac.remove(&k); }
                for e in res.effects { if let Effect::Remote { key, down: false, .. } = e { win.remove(&key); } }
            }
            assert!(mac.is_empty(), "stuck on Mac: {mac:?}");
            assert!(win.is_empty(), "stuck on Windows: {win:?}");
        }
    }
}
