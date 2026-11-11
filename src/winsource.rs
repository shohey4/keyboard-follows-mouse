//! EXPERIMENTAL Windows source: capture lifecycle around the `Router` (pure:
//! no OS calls, no clock; `now` is milliseconds from the caller).
//!
//! A low-level keyboard hook sees every keyboard on the PC, so it exists only
//! while keys are being forwarded. `Passthrough` (no hook installed, nothing
//! suppressed) is the resting state; `Capturing` (hook installed) starts on
//! the opt-in hotkey, a Flow decision, and only with the link up, and ends on
//! the hotkey, Flow handing the mouse back, the local mouse moving, the link
//! being down for `link_loss_stop_ms`, or `idle_stop_ms` without a forwarded
//! key. Every exit releases the keys held on the receiver and forgets the
//! router's held map: the hook is gone, so their key-ups would never arrive.
//!
//! Capturing is derived from the router (`effective() != Mac`), so there is no
//! second flag to drift. While the link is down it stays installed (`Held`:
//! keys are swallowed, not typed on this PC) so the hotkey still works.
//! Key codes enter `on_hook_key` and go straight into the router. Beyond that
//! it keeps only the virtual keys that were already down when capture began
//! (their repeats and key-up pass, so nothing sticks on this PC) until their
//! key-up; it counts only, and every `Debug` here redacts keys.

use std::collections::BTreeSet;

use crate::keymap::WindowsProfile;
use crate::keys::{usage_for_win_key, Usage, WinKey};
use crate::router::{Dest, Effect as RouterEffect, Hotkey, Local, Reason, Router, Target};

/// The router's `Target::Mac` is "local" here (this PC) and `Target::Windows`
/// is "remote" (the receiver); its enums keep their names.
const LOCAL: Target = Target::Mac;
const REMOTE: Target = Target::Windows;

/// VK_F13..VK_F20 for the router hotkey usages F13..F20 (HID 0x68..0x6F).
pub fn hotkey_vk(u: Usage) -> Option<u32> { (0x68..=0x6F).contains(&u.0).then(|| 0x7C + u32::from(u.0 - 0x68)) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State { Passthrough, Capturing }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why { Hotkey, Flow, LinkLoss, Idle, LocalMouse, SecureDesktop, HookFailed, Shutdown }

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Send to the receiver.
    Remote { key: WinKey, down: bool, repeat: bool },
    /// Capture started / ended (the caller installs / removes the hook).
    Capture { on: bool, why: Why },
    /// Input is swallowed (link down) / flows again (link back), while capturing.
    Holding { on: bool },
    /// Capture was refused because the link is not confirmed.
    Refused { why: Why },
}

/// The key of a `Remote` effect is redacted: nothing that prints an effect shows what was typed.
impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::Remote { down, repeat, .. } => f.debug_struct("Remote").field("key", &"<redacted>").field("down", down).field("repeat", repeat).finish(),
            Effect::Capture { on, why } => f.debug_struct("Capture").field("on", on).field("why", why).finish(),
            Effect::Holding { on } => f.debug_struct("Holding").field("on", on).finish(),
            Effect::Refused { why } => f.debug_struct("Refused").field("why", why).finish(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Config { pub link_loss_stop_ms: u64, pub idle_stop_ms: u64 }

impl Default for Config {
    fn default() -> Self { Config { link_loss_stop_ms: 3_000, idle_stop_ms: 600_000 } }
}

/// Counts only; no key is recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Key presses sent to the receiver (auto-repeat excluded).
    pub forwarded: u64,
    /// Events let through to this PC while capturing (injected, or an up we never saw go down).
    pub passes: u64,
    /// Presses with no equivalent on the wire, swallowed while capturing.
    pub unmapped: u64,
    pub captures: u64,
    pub auto_stops: u64,
}

#[derive(PartialEq, Eq)]
pub struct Hooked { pub local: Local, pub effects: Vec<Effect> }

impl std::fmt::Debug for Hooked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooked").field("local", &self.local).field("effects", &self.effects).finish() // `Effect` redacts keys
    }
}

pub struct WinSource {
    router: Router,
    hotkey_vk: u32,
    cfg: Config,
    /// Since when the link has been down while capturing.
    held_since: Option<u64>,
    /// Entry, or the last forwarded key.
    active_at: u64,
    /// Capture began from the hotkey message and the key is still down: its
    /// auto-repeat must not read as a second press.
    hotkey_pending: bool,
    /// Left by the hotkey inside the hook, which swallowed that press: the
    /// system would read the key's next auto-repeat as a fresh press and send
    /// WM_HOTKEY. Ignored until the key is seen up.
    hotkey_guard: bool,
    /// Virtual keys already down when capture began (never forwarded).
    pre_down: BTreeSet<u32>,
    counters: Counters,
}

impl std::fmt::Debug for WinSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinSource").field("state", &self.state()).field("holding", &self.holding())
            .field("held_drops", &self.router.held_drops).field("counters", &self.counters).finish()
    }
}

impl WinSource {
    /// `hotkey`: the router usage of the switch key (F13..F20).
    pub fn new(profile: WindowsProfile, hotkey: Usage, cfg: Config) -> Result<Self, String> {
        let hotkey_vk = hotkey_vk(hotkey).ok_or("the hotkey must be F13..F20")?;
        let router = Router::new(profile, Hotkey { key: hotkey, modifiers: vec![] });
        Ok(WinSource { router, hotkey_vk, cfg, held_since: None, active_at: 0, hotkey_pending: false, hotkey_guard: false, pre_down: BTreeSet::new(), counters: Counters::default() })
    }

    pub fn wants_hook(&self) -> bool { self.router.effective() != Dest::Mac }
    pub fn state(&self) -> State { if self.wants_hook() { State::Capturing } else { State::Passthrough } }
    pub fn holding(&self) -> bool { self.router.effective() == Dest::Held }
    pub fn hotkey_vk(&self) -> u32 { self.hotkey_vk }
    pub fn counters(&self) -> Counters { self.counters }
    /// Key presses swallowed while the link was down.
    pub fn held_drops(&self) -> u64 { self.router.held_drops }

    /// One keyboard event from the installed hook (`injected`: LLKHF_INJECTED or
    /// carrying our marker). Call it only while the hook is installed; outside
    /// `Capturing` (the hook is being removed) everything passes.
    pub fn on_hook_key(&mut self, vk: u32, scan: u32, extended: bool, injected: bool, down: bool, now: u64) -> Hooked {
        if !down && vk == self.hotkey_vk { self.hotkey_guard = false; }
        if !self.wants_hook() { return Hooked { local: Local::Pass, effects: vec![] }; }
        if injected {
            self.counters.passes += 1;
            return Hooked { local: Local::Pass, effects: vec![] };
        }
        // First, whatever the router is holding: the way back must always work.
        if vk == self.hotkey_vk {
            let local = |s: &mut Self| { s.counters.passes += 1; Hooked { local: Local::Pass, effects: vec![] } };
            if !down {
                // After a capture that began from the hotkey message the system never saw the
                // press end: let the release through so the key is not left logically down.
                self.hotkey_pending = false;
                self.pre_down.remove(&vk);
                return local(self);
            }
            if self.pre_down.contains(&vk) { return local(self); }
            if self.hotkey_pending { return Hooked { local: Local::Suppress, effects: vec![] }; }
            self.hotkey_guard = true;
            return Hooked { local: Local::Suppress, effects: self.leave(Why::Hotkey) };
        }
        // Keys that were down before the hook existed: their down already reached this PC, so
        // their repeats and key-up must too, or they would stay down here.
        if self.pre_down.contains(&vk) {
            if !down { self.pre_down.remove(&vk); }
            self.counters.passes += 1;
            return Hooked { local: Local::Pass, effects: vec![] };
        }
        let mapped = u8::try_from(scan).ok().filter(|c| *c != 0).and_then(|code| usage_for_win_key(WinKey::Scan { code, extended }));
        // A key with no wire equivalent still gets a router entry, so its up pairs with its down.
        let usage = mapped.unwrap_or(Usage(0xF000 | (u16::from(extended) << 8) | (vk & 0xFF) as u16));
        if mapped.is_none() && down { self.counters.unmapped += 1; }
        let r = self.router.on_key(usage, down);
        if r.local == Local::Pass { self.counters.passes += 1; }
        Hooked { local: r.local, effects: self.map(r.effects, now) }
    }

    /// The RegisterHotKey message: starts capturing (refused while the link is
    /// down). While capturing it is the way back that does not depend on the
    /// hook, which Windows may have removed without telling us.
    /// `down`: every virtual key physically down now, so those keys stay local.
    pub fn on_hotkey_message(&mut self, now: u64, down: &[u32]) -> Vec<Effect> {
        if self.hotkey_guard { return vec![]; }
        if self.wants_hook() { return self.leave(Why::Hotkey); }
        let fx = self.enter(now, Why::Hotkey, down);
        self.hotkey_pending = fx.contains(&Effect::Capture { on: true, why: Why::Hotkey });
        fx
    }

    /// The hotkey is seen up (polled by the caller; the hook may already be gone).
    pub fn on_hotkey_released(&mut self) { self.hotkey_pending = false; self.hotkey_guard = false; }

    /// Right after the hook is installed: which keys are still down. A key
    /// released since the snapshot gets no key-up from the hook, so forget it.
    pub fn on_hook_installed(&mut self, down: &[u32]) {
        self.pre_down.retain(|vk| down.contains(vk));
        if !down.contains(&self.hotkey_vk) { self.on_hotkey_released(); }
    }

    pub fn on_link(&mut self, up: bool, now: u64) -> Vec<Effect> {
        let fx = self.router.on_link(up);
        self.held_since = self.holding().then(|| self.held_since.unwrap_or(now));
        let holding: Vec<Effect> = fx.iter().filter_map(|e| match e {
            RouterEffect::DestChanged { dest: Dest::Held, .. } => Some(Effect::Holding { on: true }),
            RouterEffect::DestChanged { dest: Dest::Windows, .. } => Some(Effect::Holding { on: false }),
            _ => None,
        }).collect();
        let mut out = self.map(fx, now);
        out.extend(holding);
        out
    }

    /// Flow decided where the mouse (and so the keyboard) goes.
    pub fn on_flow(&mut self, to: Target, now: u64, down: &[u32]) -> Vec<Effect> {
        match (to, self.wants_hook()) {
            (REMOTE, false) => self.enter(now, Why::Flow, down),
            (LOCAL, true) => self.leave(Why::Flow),
            _ => vec![],
        }
    }

    /// The cursor moved on this PC: the mouse is back here.
    pub fn on_local_mouse_activity(&mut self) -> Vec<Effect> { self.leave(Why::LocalMouse) }

    /// The cursor cannot be read: the secure desktop (lock screen, UAC,
    /// Ctrl+Alt+Del) has input, which the hook never sees. Release the receiver's
    /// keys now rather than leave a Ctrl or Alt held there.
    pub fn on_secure_desktop(&mut self) -> Vec<Effect> {
        if self.wants_hook() { self.counters.auto_stops += 1; }
        self.leave(Why::SecureDesktop)
    }

    /// The hook could not be installed: do not claim to capture.
    pub fn on_hook_failed(&mut self) -> Vec<Effect> { self.leave(Why::HookFailed) }

    /// Process exit.
    pub fn shutdown(&mut self) -> Vec<Effect> { self.leave(Why::Shutdown) }

    /// Call often: the auto-stops.
    pub fn tick(&mut self, now: u64) -> Vec<Effect> {
        if !self.wants_hook() { return vec![]; }
        let why = if self.held_since.is_some_and(|t| now.saturating_sub(t) > self.cfg.link_loss_stop_ms) { Why::LinkLoss }
            else if now.saturating_sub(self.active_at) > self.cfg.idle_stop_ms { Why::Idle }
            else { return vec![] };
        self.counters.auto_stops += 1;
        self.leave(why)
    }

    fn enter(&mut self, now: u64, why: Why, down: &[u32]) -> Vec<Effect> {
        let refused = self.router.select(REMOTE, Reason::Manual).iter().any(|e| matches!(e, RouterEffect::RefusedLinkDown { .. }));
        if refused { return vec![Effect::Refused { why }]; }
        // Keys already down here stay local (the hotkey press itself is handled by `hotkey_pending`).
        self.pre_down = down.iter().copied().filter(|vk| !(why == Why::Hotkey && *vk == self.hotkey_vk)).collect();
        self.held_since = None;
        self.active_at = now;
        self.counters.captures += 1;
        vec![Effect::Capture { on: true, why }]
    }

    fn leave(&mut self, why: Why) -> Vec<Effect> {
        if !self.wants_hook() { return vec![]; }
        let ups = self.router.release_remote();
        let mut fx = self.map(ups, 0);
        self.router.select(LOCAL, Reason::Manual);
        self.router.resync(&[]); // nothing stays in the map: the hook (and its key-ups) are gone
        self.held_since = None;
        self.hotkey_pending = false;
        self.pre_down.clear();
        fx.push(Effect::Capture { on: false, why });
        fx
    }

    fn map(&mut self, fx: Vec<RouterEffect>, now: u64) -> Vec<Effect> {
        fx.into_iter().filter_map(|e| match e {
            RouterEffect::Remote { key, down, repeat } => {
                if down { self.active_at = now; }
                if down && !repeat { self.counters.forwarded += 1; }
                Some(Effect::Remote { key, down, repeat })
            }
            _ => None,
        }).collect()
    }
}

/// Is the cursor still near where capturing began? A move of `min_px` or more
/// in either axis says the mouse is back on this PC; jitter does not.
pub struct LocalMouse { anchor: Option<(i32, i32)>, min_px: i32 }

impl LocalMouse {
    pub fn new(min_px: i32) -> Self { LocalMouse { anchor: None, min_px } }

    pub fn moved_far(&mut self, capturing: bool, x: i32, y: i32) -> bool {
        if !capturing { self.anchor = None; return false; }
        let (ax, ay) = *self.anchor.get_or_insert((x, y));
        (x - ax).abs() >= self.min_px || (y - ay).abs() >= self.min_px
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::hid;

    const F13: u32 = 0x7C;
    // (vk, scan, extended) of A, C, Left Shift, Right Ctrl
    const A: (u32, u32, bool) = (0x41, 0x1E, false);
    const C: (u32, u32, bool) = (0x43, 0x2E, false);
    const LSHIFT: (u32, u32, bool) = (0xA0, 0x2A, false);

    fn src() -> WinSource { WinSource::new(WindowsProfile::identity(), Usage(0x68), Config::default()).unwrap() }
    fn capturing() -> WinSource {
        let mut s = src();
        s.on_link(true, 0);
        s.on_hotkey_message(0, &[]);
        s.on_hotkey_released();
        assert!(s.wants_hook());
        s
    }
    fn press(s: &mut WinSource, k: (u32, u32, bool), down: bool, now: u64) -> Hooked { s.on_hook_key(k.0, k.1, k.2, false, down, now) }
    fn remote(u: Usage, down: bool) -> Effect { Effect::Remote { key: crate::keys::win_key_for_usage(u).unwrap(), down, repeat: false } }

    #[test]
    fn passthrough_wants_no_hook_and_passes_everything() {
        let mut s = src();
        assert!(!s.wants_hook());
        assert_eq!(s.state(), State::Passthrough);
        assert_eq!(press(&mut s, A, true, 0), Hooked { local: Local::Pass, effects: vec![] });
        assert!(s.tick(1_000_000).is_empty());
    }

    #[test]
    fn the_hotkey_enters_capturing_only_with_the_link_up() {
        let mut s = src();
        assert_eq!(s.on_hotkey_message(0, &[]), vec![Effect::Refused { why: Why::Hotkey }]);
        assert!(!s.wants_hook());
        s.on_link(true, 10);
        assert_eq!(s.on_hotkey_message(20, &[]), vec![Effect::Capture { on: true, why: Why::Hotkey }]);
        assert!(s.wants_hook());
        assert_eq!(s.counters().captures, 1);
        assert_eq!(press(&mut s, A, true, 30), Hooked { local: Local::Suppress, effects: vec![remote(hid::A, true)] });
        assert_eq!(press(&mut s, A, false, 40).effects, vec![remote(hid::A, false)]);
        assert_eq!(s.counters().forwarded, 1);
    }

    #[test]
    fn the_hotkey_returns_even_while_held_or_link_down() {
        let mut s = capturing();
        s.on_link(false, 100);
        assert!(s.holding());
        assert_eq!(press(&mut s, A, true, 110), Hooked { local: Local::Suppress, effects: vec![] }, "held input reaches nobody");
        assert_eq!(s.held_drops(), 1);
        let r = s.on_hook_key(F13, 0x64, false, false, true, 120);
        assert_eq!(r, Hooked { local: Local::Suppress, effects: vec![Effect::Capture { on: false, why: Why::Hotkey }] });
        assert!(!s.wants_hook());
    }

    #[test]
    fn holding_a_hotkey_that_started_capture_does_not_bounce_back() {
        let mut s = src();
        s.on_link(true, 0);
        s.on_hotkey_message(0, &[]);
        // Auto-repeat arrives at the freshly installed hook, then the release.
        assert_eq!(s.on_hook_key(F13, 0x64, false, false, true, 5), Hooked { local: Local::Suppress, effects: vec![] });
        assert!(s.wants_hook());
        assert_eq!(s.on_hook_key(F13, 0x64, false, false, false, 9).local, Local::Pass);
        // A later press is a real one.
        assert!(!s.on_hook_key(F13, 0x64, false, false, true, 500).effects.is_empty());
        assert!(!s.wants_hook());
    }

    #[test]
    fn the_hotkey_message_also_returns_when_the_hook_was_removed_behind_our_back() {
        let mut s = capturing();
        assert_eq!(s.on_hotkey_message(50, &[]), vec![Effect::Capture { on: false, why: Why::Hotkey }]);
        assert!(!s.wants_hook());
    }

    #[test]
    fn link_loss_holds_then_stops_after_the_timeout() {
        let mut s = capturing();
        press(&mut s, LSHIFT, true, 10);
        let fx = s.on_link(false, 1_000);
        assert_eq!(fx, vec![remote(hid::LSHIFT, false), Effect::Holding { on: true }]);
        assert!(s.tick(1_000 + 3_000).is_empty(), "exactly the timeout is not over it");
        assert!(s.wants_hook());
        assert_eq!(s.tick(1_000 + 3_001), vec![Effect::Capture { on: false, why: Why::LinkLoss }]);
        assert!(!s.wants_hook());
        assert_eq!(s.counters().auto_stops, 1);
        assert!(s.tick(1_000_000).is_empty());
    }

    #[test]
    fn a_link_that_comes_back_in_time_resumes() {
        let mut s = capturing();
        s.on_link(false, 1_000);
        assert_eq!(s.on_link(true, 2_000), vec![Effect::Holding { on: false }]);
        assert!(s.tick(9_000).is_empty());
        assert_eq!(press(&mut s, A, true, 9_001).effects, vec![remote(hid::A, true)]);
    }

    #[test]
    fn a_key_up_without_a_seen_down_passes() {
        let mut s = capturing();
        assert_eq!(press(&mut s, A, false, 5), Hooked { local: Local::Pass, effects: vec![] });
        assert_eq!(s.counters().passes, 1);
    }

    #[test]
    fn injected_events_pass_with_no_effects() {
        let mut s = capturing();
        assert_eq!(s.on_hook_key(A.0, A.1, A.2, true, true, 5), Hooked { local: Local::Pass, effects: vec![] });
        // Even the hotkey: only a real key press switches.
        assert_eq!(s.on_hook_key(F13, 0x64, false, true, true, 6), Hooked { local: Local::Pass, effects: vec![] });
        assert!(s.wants_hook());
    }

    #[test]
    fn a_key_held_when_capture_ends_is_released_on_the_receiver() {
        let mut s = capturing();
        press(&mut s, LSHIFT, true, 10);
        press(&mut s, C, true, 11);
        let r = s.on_hook_key(F13, 0x64, false, false, true, 20);
        assert_eq!(r.effects, vec![remote(hid::C, false), remote(hid::LSHIFT, false), Effect::Capture { on: false, why: Why::Hotkey }], "modifiers last");
    }

    #[test]
    fn a_key_held_across_a_stop_forwards_again_next_time() {
        let mut s = capturing();
        press(&mut s, A, true, 10);
        s.shutdown();
        s.on_hotkey_message(100, &[]);
        s.on_hotkey_released();
        assert_eq!(press(&mut s, A, true, 110).effects, vec![remote(hid::A, true)], "not swallowed as a repeat of the old route");
    }

    #[test]
    fn keys_down_at_entry_pass_their_repeats_and_key_up_then_forward_again() {
        // A letter held when capture begins by Flow, and Shift when it begins by the hotkey message.
        for (via_flow, held) in [(true, vec![A.0]), (false, vec![LSHIFT.0, A.0])] {
            let mut s = src();
            s.on_link(true, 0);
            if via_flow { s.on_flow(REMOTE, 5, &held); } else { s.on_hotkey_message(5, &held); }
            assert!(s.wants_hook());
            s.on_hook_installed(&held);
            assert_eq!(press(&mut s, A, true, 6), Hooked { local: Local::Pass, effects: vec![] }, "auto-repeat of a key whose down this PC already got");
            assert_eq!(press(&mut s, A, false, 7), Hooked { local: Local::Pass, effects: vec![] }, "its up too, or it sticks down here");
            assert_eq!(press(&mut s, A, true, 8).effects, vec![remote(hid::A, true)], "the next press is a new one");
        }
    }

    #[test]
    fn a_key_released_before_the_hook_was_installed_is_not_left_local() {
        let mut s = src();
        s.on_link(true, 0);
        s.on_flow(REMOTE, 5, &[LSHIFT.0, A.0]);
        s.on_hook_installed(&[LSHIFT.0]); // A went up in between and no up will come
        assert_eq!(press(&mut s, A, true, 6).effects, vec![remote(hid::A, true)]);
        assert_eq!(press(&mut s, LSHIFT, false, 7).local, Local::Pass);
    }

    #[test]
    fn the_hotkeys_own_release_passes_after_a_message_entry() {
        let mut s = src();
        s.on_link(true, 0);
        s.on_hotkey_message(0, &[F13]);
        assert_eq!(s.on_hook_key(F13, 0x64, false, false, true, 5).local, Local::Suppress, "repeat is swallowed");
        assert_eq!(s.on_hook_key(F13, 0x64, false, false, false, 9), Hooked { local: Local::Pass, effects: vec![] });
        assert!(s.wants_hook());
    }

    #[test]
    fn holding_the_hotkey_after_leaving_does_not_re_enter() {
        let mut s = capturing();
        assert!(!s.on_hook_key(F13, 0x64, false, false, true, 5).effects.is_empty());
        assert!(!s.wants_hook());
        // The typematic repeat becomes a WM_HOTKEY: ignored while the key is still down.
        assert!(s.on_hotkey_message(600, &[F13]).is_empty());
        assert!(!s.wants_hook());
        s.on_hotkey_released();
        assert_eq!(s.on_hotkey_message(900, &[F13]), vec![Effect::Capture { on: true, why: Why::Hotkey }]);
        // A release seen by the hook clears the guard too.
        s.on_hook_key(F13, 0x64, false, false, true, 910);
        s.on_hook_key(F13, 0x64, false, false, false, 920);
        assert!(!s.on_hook_key(F13, 0x64, false, false, true, 930).effects.is_empty());
        s.on_hook_key(F13, 0x64, false, false, false, 940);
        assert_eq!(s.on_hotkey_message(1_000, &[]).len(), 1);
    }

    #[test]
    fn the_secure_desktop_releases_the_receivers_keys() {
        let mut s = capturing();
        press(&mut s, LSHIFT, true, 5);
        press(&mut s, A, true, 6);
        assert_eq!(s.on_secure_desktop(), vec![remote(hid::A, false), remote(hid::LSHIFT, false), Effect::Capture { on: false, why: Why::SecureDesktop }]);
        assert!(!s.wants_hook());
        assert!(s.on_secure_desktop().is_empty(), "nothing to do while at rest");
        // Also while the link is down and input is held.
        let mut s = capturing();
        s.on_link(false, 10);
        assert_eq!(s.on_secure_desktop(), vec![Effect::Capture { on: false, why: Why::SecureDesktop }]);
    }

    #[test]
    fn flow_and_the_local_mouse_return_to_local() {
        let mut s = src();
        assert_eq!(s.on_flow(REMOTE, 0, &[]), vec![Effect::Refused { why: Why::Flow }]);
        s.on_link(true, 0);
        assert_eq!(s.on_flow(REMOTE, 1, &[]), vec![Effect::Capture { on: true, why: Why::Flow }]);
        assert_eq!(s.on_flow(LOCAL, 2, &[]), vec![Effect::Capture { on: false, why: Why::Flow }]);
        s.on_flow(REMOTE, 3, &[]);
        assert_eq!(s.on_local_mouse_activity(), vec![Effect::Capture { on: false, why: Why::LocalMouse }]);
        assert!(s.on_local_mouse_activity().is_empty());
    }

    #[test]
    fn idle_stops_but_a_forwarded_key_resets_the_timer() {
        let mut s = capturing();
        let idle = Config::default().idle_stop_ms;
        press(&mut s, A, true, idle - 1);
        press(&mut s, A, false, idle);
        assert!(s.tick(idle - 1 + idle).is_empty());
        assert_eq!(s.tick(idle - 1 + idle + 1), vec![Effect::Capture { on: false, why: Why::Idle }]);
    }

    #[test]
    fn unmapped_keys_are_swallowed_and_counted() {
        let mut s = capturing();
        let numpad_enter = (0x0D, 0x1C, true);
        assert_eq!(press(&mut s, numpad_enter, true, 5), Hooked { local: Local::Suppress, effects: vec![] });
        assert_eq!(press(&mut s, numpad_enter, false, 6), Hooked { local: Local::Suppress, effects: vec![] });
        assert_eq!(s.counters().unmapped, 1);
    }

    #[test]
    fn a_failed_hook_install_does_not_claim_capture() {
        let mut s = capturing();
        assert_eq!(s.on_hook_failed(), vec![Effect::Capture { on: false, why: Why::HookFailed }]);
        assert!(!s.wants_hook());
    }

    #[test]
    fn debug_and_counters_never_contain_key_codes() {
        let mut s = capturing();
        let mut shown = String::new();
        for k in [C, C] {
            for down in [true, false] { let h = press(&mut s, k, down, 5); shown += &format!("{h:?} {:?} ", h.effects); }
        }
        let text = format!("{s:?} {:?} {shown}", s.counters());
        for banned in ["46", "67", "0x2e", "0x43", "Scan", "Usage", "WinKey", "Vk", "code"] { assert!(!text.contains(banned), "{banned} in {text}"); }
        assert!(text.contains("forwarded: 2"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }

    #[test]
    fn only_f13_to_f20_can_be_the_hotkey() {
        assert!(WinSource::new(WindowsProfile::identity(), hid::A, Config::default()).is_err());
        assert_eq!(hotkey_vk(Usage(0x6F)), Some(0x83));
    }

    #[test]
    fn local_mouse_ignores_jitter_and_resets_when_not_capturing() {
        let mut m = LocalMouse::new(60);
        assert!(!m.moved_far(false, 5, 5));
        assert!(!m.moved_far(true, 100, 100));
        assert!(!m.moved_far(true, 110, 95));
        assert!(m.moved_far(true, 100, 170));
        assert!(!m.moved_far(false, 100, 170));
        assert!(!m.moved_far(true, 100, 170), "a fresh anchor");
    }
}
