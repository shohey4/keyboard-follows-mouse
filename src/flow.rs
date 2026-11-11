//! v1 sketch: infer which host Flow gave the MX4 to, from observable mouse
//! activity only (no Logi Options+ internals). Pure; times in ms.
//!
//! Mac -> Windows: the Mac cursor hits the configured edge, then Windows
//! reports mouse activity within `window_ms`. Windows activity without a
//! preceding edge hit (someone else on that PC, a jiggle) is ignored.
//! Windows -> Mac: real mouse activity on the Mac (Flow returned the cursor, or
//! the trackpad was used). Any switch is held for `dwell_ms` against flapping.
//! A manual pin overrides everything until cleared. Link loss only forgets a
//! pending edge: what happens to keys then is the router's `LinkLossPolicy`
//! (hold by default), so the follower must not "decide Mac" on its own.

use crate::router::Target;

#[derive(Clone, Copy, Debug)]
pub struct FlowConfig { pub window_ms: u64, pub dwell_ms: u64 }

impl Default for FlowConfig {
    fn default() -> Self { FlowConfig { window_ms: 400, dwell_ms: 150 } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Mac cursor at the edge facing the Windows PC.
    MacEdge,
    /// Mac mouse moved / clicked (not merely parked at the edge).
    MacActivity,
    /// Windows receiver saw mouse input.
    WinActivity,
    LinkDown,
    Pin(Target),
    Unpin,
}

pub struct FlowFollower {
    cfg: FlowConfig,
    active: Target,
    edge_at: Option<u64>,
    switched_at: Option<u64>,
    pinned: Option<Target>,
}

impl FlowFollower {
    pub fn new(cfg: FlowConfig) -> Self {
        FlowFollower { cfg, active: Target::Mac, edge_at: None, switched_at: None, pinned: None }
    }

    pub fn active(&self) -> Target { self.pinned.unwrap_or(self.active) }

    /// Returns the new target when the decision changes.
    pub fn on(&mut self, signal: Signal, now: u64) -> Option<Target> {
        let before = self.active();
        match signal {
            Signal::Pin(t) => self.pinned = Some(t),
            Signal::Unpin => self.pinned = None,
            Signal::LinkDown => self.edge_at = None,
            Signal::MacEdge => self.edge_at = Some(now),
            Signal::WinActivity => {
                let recent_edge = self.edge_at.is_some_and(|t| now.saturating_sub(t) <= self.cfg.window_ms);
                if self.active == Target::Mac && recent_edge && self.settled(now) { self.switch(Target::Windows, now); }
            }
            Signal::MacActivity => {
                self.edge_at = None;
                if self.active == Target::Windows && self.settled(now) { self.switch(Target::Mac, now); }
            }
        }
        let after = self.active();
        (before != after).then_some(after)
    }

    fn settled(&self, now: u64) -> bool {
        self.switched_at.is_none_or(|t| now.saturating_sub(t) >= self.cfg.dwell_ms)
    }

    fn switch(&mut self, t: Target, now: u64) {
        self.active = t;
        self.switched_at = Some(now);
        self.edge_at = None;
    }
}

// ---- stage 7a: observing the Mac pointer and recording decisions ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge { Left, Right, Top, Bottom }

impl std::str::FromStr for Edge {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s { "left" => Ok(Edge::Left), "right" => Ok(Edge::Right), "top" => Ok(Edge::Top), "bottom" => Ok(Edge::Bottom),
                  _ => Err(format!("--flow-edge must be left|right|top|bottom, not {s:?}")) }
    }
}

/// A display in global points (CoreGraphics: origin top-left, y grows down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect { pub x: f64, pub y: f64, pub w: f64, pub h: f64 }

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool { x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h }
}

/// Index of the display under the point, if any.
pub fn display_of(displays: &[Rect], x: f64, y: f64) -> Option<usize> { displays.iter().position(|r| r.contains(x, y)) }

/// The candidate display whose *outer* `edge` the point is pinned against:
/// the last pixel row/column, with no other display just beyond it (so a
/// shared border between two Mac displays is not an exit to Windows).
pub fn edge_display(displays: &[Rect], candidates: &[usize], edge: Edge, x: f64, y: f64) -> Option<usize> {
    let i = display_of(displays, x, y)?;
    if !candidates.contains(&i) { return None; }
    let r = displays[i];
    let (at, beyond) = match edge {
        Edge::Right => (x >= r.x + r.w - 1.0, (x + 1.0, y)),
        Edge::Left => (x <= r.x, (x - 1.0, y)),
        Edge::Bottom => (y >= r.y + r.h - 1.0, (x, y + 1.0)),
        Edge::Top => (y <= r.y, (x, y - 1.0)),
    };
    (at && display_of(displays, beyond.0, beyond.1).is_none()).then_some(i)
}

/// Percent of display `i`'s `edge` (sampled every 4 points) with no other
/// display beyond it: 100 = facing outward, 0 = shared with another display.
pub fn outer_edge_share(ds: &[Rect], i: usize, edge: Edge) -> u32 {
    let r = ds[i];
    let along = if matches!(edge, Edge::Left | Edge::Right) { r.h } else { r.w };
    let steps = ((along / 4.0) as usize).max(1);
    let outer = (0..steps).filter(|k| {
        let t = *k as f64 * 4.0;
        let (x, y) = match edge { Edge::Right => (r.x + r.w - 1.0, r.y + t), Edge::Left => (r.x, r.y + t), Edge::Bottom => (r.x + t, r.y + r.h - 1.0), Edge::Top => (r.x + t, r.y) };
        edge_display(ds, &[i], edge, x, y).is_some()
    }).count();
    (outer * 100 / steps) as u32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerEvent {
    /// Arrived at the outer edge of this display (once per arrival).
    EdgeArrived(usize),
    /// Moved anywhere else; the display under the pointer, if any.
    Moved(Option<usize>),
}

/// Turns pointer samples into events. Sliding along the edge, or not moving,
/// is no event. The first sample is only a baseline.
#[derive(Default)]
pub struct PointerTracker { last: Option<(f64, f64)>, at_edge: Option<usize>, edge_pt: Option<(f64, f64)> }

impl PointerTracker {
    pub fn on_sample(&mut self, displays: &[Rect], candidates: &[usize], edge: Edge, x: f64, y: f64) -> Option<PointerEvent> {
        let first = self.last.is_none();
        if self.last == Some((x, y)) { return None; }
        self.last = Some((x, y));
        let at = edge_display(displays, candidates, edge, x, y);
        let was = std::mem::replace(&mut self.at_edge, at);
        if first { return None; }
        match at {
            Some(i) if was != Some(i) => { self.edge_pt = Some((x, y)); Some(PointerEvent::EdgeArrived(i)) }
            Some(_) => None,
            None => Some(PointerEvent::Moved(display_of(displays, x, y))),
        }
    }

    /// Distance in points from the last edge arrival to the latest sample (a
    /// magnitude only; no coordinates leave this struct).
    pub fn from_edge_pt(&self) -> Option<f64> {
        let ((ex, ey), (x, y)) = (self.edge_pt?, self.last?);
        Some(((x - ex).powi(2) + (y - ey).powi(2)).sqrt())
    }
}

/// What the Windows receiver saw at its outer left edge (the side facing the
/// Mac), from cursor polls. Counted and sent in the Ack; never coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinEdgeEvent {
    /// The cursor jumped onto the outer left edge (Flow brought it in).
    Entered,
    /// The cursor left the outer left edge inward (control is on Windows).
    Departed,
    /// The cursor moved onto the outer left edge by itself (heading to the Mac).
    Reached,
}

/// Receiver side: turns cursor polls into `WinEdgeEvent`s at the receiver's
/// outer `edge` that faces the keyboard's host (left by default: the receiver
/// sits to the right), on the listed `displays` (None = all). A step of at
/// least `jump_px` between two polls that lands on the edge counts as a jump.
/// The names say "Win" for history; the receiver may be any OS.
pub struct EntryTracker { last: Option<(f64, f64)>, at_edge: bool, pub jump_px: f64, pub edge: Edge, pub displays: Option<Vec<usize>> }

impl Default for EntryTracker { fn default() -> Self { EntryTracker { last: None, at_edge: false, jump_px: 150.0, edge: Edge::Left, displays: None } } }

impl EntryTracker {
    /// Watch `edge` of the given displays (indices into the monitor list; None = all).
    pub fn facing(edge: Edge, displays: Option<Vec<usize>>) -> Self { EntryTracker { edge, displays, ..Default::default() } }

    pub fn on_sample(&mut self, monitors: &[Rect], x: f64, y: f64) -> Option<WinEdgeEvent> {
        let all: Vec<usize> = self.displays.clone().unwrap_or_else(|| (0..monitors.len()).collect());
        let at = edge_display(monitors, &all, self.edge, x, y).is_some();
        let prev = self.last.replace((x, y));
        let was = std::mem::replace(&mut self.at_edge, at);
        let (px, py) = prev?;
        match (was, at) {
            (false, true) if ((x - px).powi(2) + (y - py).powi(2)).sqrt() >= self.jump_px => Some(WinEdgeEvent::Entered),
            (false, true) => Some(WinEdgeEvent::Reached),
            (true, false) => Some(WinEdgeEvent::Departed),
            _ => None,
        }
    }
}

/// Rule E (Easy-Switch, also Flow): the MX4 appearing in or leaving the
/// Windows PC's device list. Edge-triggered: `None` unless it arrived or left
/// since the last report. When both happened between two reports, where it is
/// now decides. Leaving Windows means the Mac, also when it went to a third
/// host: the keyboard stays where it can be seen.
pub fn rule_e(arrivals: u32, departures: u32, present_now: bool) -> Option<Target> {
    if arrivals == 0 && departures == 0 { return None; }
    Some(if present_now { Target::Windows } else { Target::Mac })
}

/// Which rule a decision came from. A = `FlowFollower` as configured (what the
/// router would use today). C = the Mac edge and the Windows entry together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule { A, C }

/// What the observer wants logged (times, counts and distances only; no
/// coordinates, no keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    Edge { display: usize },
    /// Rule A: no Windows activity within its window after this edge.
    EdgeExpired { display: usize },
    /// Windows mouse activity after a quiet spell. `ignored`: rule A stayed on
    /// the Mac (a misjudgment candidate if Flow did move). `since_edge_ms`: time
    /// since the last Mac edge, however old.
    WinBurst { ignored: bool, since_edge_ms: Option<u64> },
    /// The Windows receiver reported an event at its left edge.
    WinEdge { event: WinEdgeEvent, since_edge_ms: Option<u64> },
    ToWindows { rule: Rule, display: Option<usize>, after_edge_ms: Option<u64> },
    /// Mac activity took the decision back; the display the pointer showed up on.
    ToMac { rule: Rule, display: Option<usize>, after_windows_ms: Option<u64>, from_edge_pt: Option<u32> },
    /// Rule C kept Windows despite a Mac move (first one per switch).
    MacMoveHeld { after_windows_ms: u64, from_edge_pt: Option<u32> },
    LinkDown,
}

/// Rule C parameters.
#[derive(Clone, Copy, Debug)]
pub struct RuleC {
    /// Windows entry at most this long after the Mac edge (or `entry_before_ms` before it).
    pub pair_ms: u64,
    pub entry_before_ms: u64,
    /// Back to the Mac: a Mac move within this long after Windows reported `Reached`...
    pub exit_pair_ms: u64,
    /// ...or a Mac move this far from the edge with no Windows activity for `win_quiet_ms`.
    pub far_pt: f64,
    pub win_quiet_ms: u64,
    /// Mac moves closer than this to the edge never cancel a pending edge.
    pub near_pt: f64,
    /// A `Reached` arriving this soon after a held Mac move returns to the Mac at
    /// once (the Windows report can trail the Mac move by a few ms).
    pub late_exit_ms: u64,
}

impl Default for RuleC {
    fn default() -> Self { RuleC { pair_ms: 1500, entry_before_ms: 300, exit_pair_ms: 1500, far_pt: 100.0, win_quiet_ms: 1000, near_pt: 40.0, late_exit_ms: 300 } }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Decisions { pub to_windows: Vec<u64>, pub to_mac: Vec<u64>, pub to_mac_unknown_display: u64 }

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Counts {
    pub edges: Vec<u64>,
    /// Rule A: edges with no Windows activity in time / cancelled by Mac movement.
    pub edges_expired: u64,
    pub edges_cancelled: u64,
    pub win_bursts: u64,
    pub win_bursts_ignored: u64,
    /// Windows activity reports, and the gaps between them inside a burst.
    pub win_reports: u64,
    pub win_gap_sum_ms: u64,
    pub win_gaps: u64,
    pub win_entered: u64,
    pub win_departed: u64,
    pub win_reached: u64,
    pub a: Decisions,
    pub c: Decisions,
    pub c_mac_moves_held: u64,
}

/// Stage 7a: runs rule A and rule C side by side on real observations and only
/// records what each would decide. It never touches the route.
pub struct Observer {
    a: FlowFollower,
    a_switched_at: Option<u64>,
    a_window_ms: u64,
    c_cfg: RuleC,
    c_active: Target,
    c_edge: Option<(usize, u64)>,
    c_entry: Option<u64>,
    c_reached: Option<u64>,
    c_switched_at: Option<u64>,
    c_held_this_switch: bool,
    /// The last Mac move rule C held: when, on which display, how far from the edge.
    c_held_move: Option<(u64, Option<usize>, Option<u32>)>,
    dwell_ms: u64,
    displays: usize,
    burst_quiet_ms: u64,
    last_win_ms: Option<u64>,
    last_edge: Option<(usize, u64)>,
    /// Rule A's edge still waiting for Windows activity.
    pending_edge: Option<(usize, u64)>,
    pub counts: Counts,
}

impl Observer {
    pub fn new(cfg: FlowConfig, displays: usize) -> Self { Self::with_rule_c(cfg, RuleC::default(), displays) }

    pub fn with_rule_c(cfg: FlowConfig, c_cfg: RuleC, displays: usize) -> Self {
        let dec = || Decisions { to_windows: vec![0; displays], to_mac: vec![0; displays], to_mac_unknown_display: 0 };
        Observer {
            a: FlowFollower::new(cfg), a_switched_at: None, a_window_ms: cfg.window_ms,
            c_cfg, c_active: Target::Mac, c_edge: None, c_entry: None, c_reached: None, c_switched_at: None, c_held_this_switch: false, c_held_move: None,
            dwell_ms: cfg.dwell_ms, displays, burst_quiet_ms: 500, last_win_ms: None, last_edge: None, pending_edge: None,
            counts: Counts { edges: vec![0; displays], a: dec(), c: dec(), ..Counts::default() },
        }
    }

    /// Rule A's decision (what the router would follow today).
    pub fn active(&self) -> Target { self.a.active() }
    pub fn active_c(&self) -> Target { self.c_active }

    fn count(&mut self, rule: Rule, to: Target, d: Option<usize>) {
        let n = self.displays;
        let dec = match rule { Rule::A => &mut self.counts.a, Rule::C => &mut self.counts.c };
        let v = if to == Target::Windows { &mut dec.to_windows } else { &mut dec.to_mac };
        match d { Some(i) if i < n => v[i] += 1, _ if to == Target::Mac => dec.to_mac_unknown_display += 1, _ => {} }
    }

    fn c_to_windows(&mut self, now: u64) -> Note {
        let (display, after) = self.c_edge.map_or((None, None), |(d, t)| (Some(d), Some(now.saturating_sub(t))));
        self.c_active = Target::Windows;
        self.c_switched_at = Some(now);
        self.c_held_this_switch = false;
        self.c_held_move = None;
        self.c_edge = None;
        self.c_entry = None;
        self.c_reached = None;
        self.count(Rule::C, Target::Windows, display);
        Note::ToWindows { rule: Rule::C, display, after_edge_ms: after }
    }

    /// `from_edge_pt`: `PointerTracker::from_edge_pt` for this sample.
    pub fn on_pointer(&mut self, ev: PointerEvent, now: u64, from_edge_pt: Option<f64>) -> Vec<Note> {
        let pt = from_edge_pt.map(|d| d.round().min(f64::from(u32::MAX)) as u32);
        let mut notes = vec![];
        match ev {
            PointerEvent::EdgeArrived(d) => {
                if d < self.displays { self.counts.edges[d] += 1; }
                self.last_edge = Some((d, now));
                if self.a.active() == Target::Mac { self.pending_edge = Some((d, now)); }
                self.a.on(Signal::MacEdge, now);
                notes.push(Note::Edge { display: d });
                if self.c_active == Target::Mac {
                    self.c_edge = Some((d, now));
                    // The Windows entry may be reported just before the Mac edge is sampled.
                    if self.c_entry.is_some_and(|t| now.saturating_sub(t) <= self.c_cfg.entry_before_ms) { notes.push(self.c_to_windows(now)); }
                }
            }
            PointerEvent::Moved(d) => {
                if self.pending_edge.take().is_some() { self.counts.edges_cancelled += 1; }
                if self.a.on(Signal::MacActivity, now) == Some(Target::Mac) {
                    self.count(Rule::A, Target::Mac, d);
                    notes.push(Note::ToMac { rule: Rule::A, display: d, after_windows_ms: self.a_switched_at.map(|t| now.saturating_sub(t)), from_edge_pt: pt });
                }
                let far = |p: f64| from_edge_pt.is_none_or(|x| x >= p);
                match self.c_active {
                    Target::Mac => if far(self.c_cfg.near_pt) { self.c_edge = None; },
                    Target::Windows => {
                        let since = self.c_switched_at.map_or(u64::MAX, |t| now.saturating_sub(t));
                        let exited = self.c_reached.is_some_and(|t| now.saturating_sub(t) <= self.c_cfg.exit_pair_ms);
                        let quiet = self.last_win_ms.is_none_or(|t| now.saturating_sub(t) >= self.c_cfg.win_quiet_ms);
                        if since >= self.dwell_ms && (exited || (far(self.c_cfg.far_pt) && quiet)) {
                            self.c_active = Target::Mac;
                            self.c_reached = None;
                            self.count(Rule::C, Target::Mac, d);
                            notes.push(Note::ToMac { rule: Rule::C, display: d, after_windows_ms: Some(since), from_edge_pt: pt });
                        } else {
                            self.counts.c_mac_moves_held += 1;
                            self.c_held_move = Some((now, d, pt));
                            if !std::mem::replace(&mut self.c_held_this_switch, true) {
                                notes.push(Note::MacMoveHeld { after_windows_ms: since, from_edge_pt: pt });
                            }
                        }
                    }
                }
            }
        }
        notes
    }

    /// The receiver's cursor moved (one report per changed count).
    pub fn on_win_activity(&mut self, now: u64) -> Vec<Note> {
        let mut notes = vec![];
        self.counts.win_reports += 1;
        let burst = self.last_win_ms.is_none_or(|t| now.saturating_sub(t) >= self.burst_quiet_ms);
        if !burst && let Some(t) = self.last_win_ms { self.counts.win_gap_sum_ms += now.saturating_sub(t); self.counts.win_gaps += 1; }
        self.last_win_ms = Some(now);
        let before = self.a.active();
        let decided = self.a.on(Signal::WinActivity, now);
        if burst {
            let ignored = before == Target::Mac && decided.is_none();
            self.counts.win_bursts += 1;
            if ignored { self.counts.win_bursts_ignored += 1; }
            notes.push(Note::WinBurst { ignored, since_edge_ms: self.last_edge.map(|(_, t)| now.saturating_sub(t)) });
        }
        if decided == Some(Target::Windows) {
            self.pending_edge = None;
            self.a_switched_at = Some(now);
            let (display, after_edge_ms) = self.last_edge.map_or((None, None), |(d, t)| (Some(d), Some(now.saturating_sub(t))));
            self.count(Rule::A, Target::Windows, display);
            notes.push(Note::ToWindows { rule: Rule::A, display, after_edge_ms });
        }
        notes
    }

    /// The receiver reported a left-edge event.
    pub fn on_win_edge(&mut self, event: WinEdgeEvent, now: u64) -> Vec<Note> {
        let mut notes = vec![Note::WinEdge { event, since_edge_ms: self.last_edge.map(|(_, t)| now.saturating_sub(t)) }];
        match event {
            WinEdgeEvent::Entered | WinEdgeEvent::Departed => {
                if event == WinEdgeEvent::Entered { self.counts.win_entered += 1 } else { self.counts.win_departed += 1 }
                self.c_entry = Some(now);
                if self.c_active == Target::Mac && self.c_edge.is_some_and(|(_, t)| now.saturating_sub(t) <= self.c_cfg.pair_ms) {
                    notes.push(self.c_to_windows(now));
                }
            }
            WinEdgeEvent::Reached => {
                self.counts.win_reached += 1;
                self.c_reached = Some(now);
                // The Mac move came first and was held: the exit settles it now.
                if self.c_active == Target::Windows
                    && let Some((t, d, pt)) = self.c_held_move
                    && now.saturating_sub(t) <= self.c_cfg.late_exit_ms
                    && self.c_switched_at.is_some_and(|s| t.saturating_sub(s) >= self.dwell_ms) {
                    self.c_active = Target::Mac;
                    self.c_reached = None;
                    self.c_held_move = None;
                    self.count(Rule::C, Target::Mac, d);
                    notes.push(Note::ToMac { rule: Rule::C, display: d, after_windows_ms: self.c_switched_at.map(|s| now.saturating_sub(s)), from_edge_pt: pt });
                }
            }
        }
        notes
    }

    /// Call every poll: reports rule A's edges that saw no Windows activity in time.
    pub fn tick(&mut self, now: u64) -> Vec<Note> {
        match self.pending_edge {
            Some((d, t)) if now.saturating_sub(t) > self.a_window_ms => {
                self.pending_edge = None;
                self.counts.edges_expired += 1;
                vec![Note::EdgeExpired { display: d }]
            }
            _ => vec![],
        }
    }

    pub fn on_link_down(&mut self, now: u64) -> Vec<Note> {
        self.pending_edge = None;
        self.c_edge = None;
        self.c_entry = None;
        self.a.on(Signal::LinkDown, now);
        vec![Note::LinkDown]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Signal::*;

    fn f() -> FlowFollower { FlowFollower::new(FlowConfig::default()) }

    #[test]
    fn edge_then_windows_activity_switches() {
        let mut x = f();
        assert_eq!(x.on(MacEdge, 1000), None);
        assert_eq!(x.on(WinActivity, 1200), Some(Target::Windows));
        assert_eq!(x.on(MacActivity, 5000), Some(Target::Mac));
    }

    #[test]
    fn windows_activity_without_edge_is_ignored() {
        let mut x = f();
        assert_eq!(x.on(WinActivity, 1000), None);
        x.on(MacEdge, 1000);
        assert_eq!(x.on(WinActivity, 1000 + 401), None); // too late
    }

    #[test]
    fn mac_movement_cancels_a_pending_edge() {
        let mut x = f();
        x.on(MacEdge, 1000);
        x.on(MacActivity, 1100); // bumped the edge and came back
        assert_eq!(x.on(WinActivity, 1200), None);
    }

    #[test]
    fn dwell_prevents_flapping() {
        let mut x = f();
        x.on(MacEdge, 1000);
        x.on(WinActivity, 1010);
        assert_eq!(x.on(MacActivity, 1100), None); // within 150 ms
        assert_eq!(x.on(MacActivity, 1200), Some(Target::Mac));
    }

    #[test]
    fn pin_overrides_until_cleared() {
        let mut x = f();
        assert_eq!(x.on(Pin(Target::Windows), 0), Some(Target::Windows));
        assert_eq!(x.on(MacActivity, 1000), None); // inference cannot override a pin
        assert_eq!(x.on(Unpin, 2000), Some(Target::Mac));
        x.on(MacEdge, 3000);
        assert_eq!(x.on(WinActivity, 3100), Some(Target::Windows));
        assert_eq!(x.on(Pin(Target::Mac), 3200), Some(Target::Mac));
        assert_eq!(x.on(Unpin, 3300), Some(Target::Windows));
    }

    #[test]
    fn link_down_forgets_the_pending_edge_but_decides_nothing() {
        let mut x = f();
        x.on(MacEdge, 1000);
        assert_eq!(x.on(LinkDown, 1100), None);
        assert_eq!(x.on(WinActivity, 1200), None);
        assert_eq!(x.active(), Target::Mac);
    }

    // ---- stage 7a ----

    /// Built-in display below an ultrawide (both have a right edge facing Windows).
    fn two() -> Vec<Rect> { vec![Rect { x: 0.0, y: 0.0, w: 1512.0, h: 982.0 }, Rect { x: -500.0, y: -1440.0, w: 3440.0, h: 1440.0 }] }

    #[test]
    fn outer_right_edges_of_both_displays() {
        let d = two();
        assert_eq!(edge_display(&d, &[0, 1], Edge::Right, 1511.0, 500.0), Some(0));
        assert_eq!(edge_display(&d, &[0, 1], Edge::Right, 2939.0, -700.0), Some(1));
        assert_eq!(edge_display(&d, &[1], Edge::Right, 1511.0, 500.0), None, "not a candidate");
        assert_eq!(edge_display(&d, &[0, 1], Edge::Right, 1510.0, 500.0), None, "one point short of the edge");
        assert_eq!(edge_display(&d, &[0, 1], Edge::Left, 0.0, 500.0), Some(0));
    }

    #[test]
    fn a_shared_border_is_not_an_exit() {
        let d = vec![Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 }, Rect { x: 100.0, y: 0.0, w: 100.0, h: 100.0 }];
        assert_eq!(edge_display(&d, &[0, 1], Edge::Right, 99.0, 50.0), None);
        assert_eq!(edge_display(&d, &[0, 1], Edge::Right, 199.0, 50.0), Some(1));
    }

    #[test]
    fn tracker_reports_arrivals_once_and_moves() {
        let (d, c) = (two(), [0, 1]);
        let mut t = PointerTracker::default();
        assert_eq!(t.on_sample(&d, &c, Edge::Right, 100.0, 100.0), None, "baseline");
        assert_eq!(t.on_sample(&d, &c, Edge::Right, 100.0, 100.0), None, "no movement");
        assert_eq!(t.on_sample(&d, &c, Edge::Right, 1511.0, 100.0), Some(PointerEvent::EdgeArrived(0)));
        assert_eq!(t.on_sample(&d, &c, Edge::Right, 1511.0, 140.0), None, "sliding along the edge");
        assert_eq!(t.on_sample(&d, &c, Edge::Right, 2939.0, -10.0), Some(PointerEvent::EdgeArrived(1)));
        assert_eq!(t.on_sample(&d, &c, Edge::Right, 400.0, 300.0), Some(PointerEvent::Moved(Some(0))));
    }

    #[test]
    fn observer_records_a_round_trip_and_where_the_pointer_came_back() {
        let mut o = Observer::new(FlowConfig::default(), 2);
        assert_eq!(o.on_pointer(PointerEvent::EdgeArrived(1), 1000, Some(0.0)), vec![Note::Edge { display: 1 }]);
        assert_eq!(o.on_win_edge(WinEdgeEvent::Entered, 1080), vec![Note::WinEdge { event: WinEdgeEvent::Entered, since_edge_ms: Some(80) },
                                                                     Note::ToWindows { rule: Rule::C, display: Some(1), after_edge_ms: Some(80) }]);
        let n = o.on_win_activity(1100);
        assert_eq!(n, vec![Note::WinBurst { ignored: false, since_edge_ms: Some(100) }, Note::ToWindows { rule: Rule::A, display: Some(1), after_edge_ms: Some(100) }]);
        o.on_win_activity(1150);
        assert_eq!(o.on_win_edge(WinEdgeEvent::Reached, 4900), vec![Note::WinEdge { event: WinEdgeEvent::Reached, since_edge_ms: Some(3900) }]);
        let n = o.on_pointer(PointerEvent::Moved(Some(0)), 5000, Some(900.0));
        assert_eq!(n, vec![Note::ToMac { rule: Rule::A, display: Some(0), after_windows_ms: Some(3900), from_edge_pt: Some(900) },
                           Note::ToMac { rule: Rule::C, display: Some(0), after_windows_ms: Some(3920), from_edge_pt: Some(900) }]);
        assert_eq!(o.counts.edges, vec![0, 1]);
        assert_eq!((o.counts.a.to_windows.clone(), o.counts.a.to_mac.clone()), (vec![0, 1], vec![1, 0]));
        assert_eq!((o.counts.c.to_windows.clone(), o.counts.c.to_mac.clone()), (vec![0, 1], vec![1, 0]));
        assert_eq!((o.counts.win_reports, o.counts.win_gaps, o.counts.win_gap_sum_ms), (2, 1, 50));
    }

    #[test]
    fn windows_activity_without_an_edge_is_a_recorded_candidate() {
        let mut o = Observer::new(FlowConfig::default(), 2);
        assert_eq!(o.on_win_activity(1000), vec![Note::WinBurst { ignored: true, since_edge_ms: None }]);
        assert_eq!(o.on_win_activity(2000), vec![Note::WinBurst { ignored: true, since_edge_ms: None }], "a new burst after a quiet spell");
        assert_eq!((o.counts.win_bursts, o.counts.win_bursts_ignored), (2, 2));
        assert_eq!((o.active(), o.active_c()), (Target::Mac, Target::Mac));
    }

    #[test]
    fn link_down_drops_the_pending_edge() {
        let mut o = Observer::new(FlowConfig::default(), 2);
        o.on_pointer(PointerEvent::EdgeArrived(0), 1000, Some(0.0));
        o.on_link_down(1050);
        assert_eq!(o.on_win_activity(1100), vec![Note::WinBurst { ignored: true, since_edge_ms: Some(100) }]);
        assert_eq!(o.on_win_edge(WinEdgeEvent::Entered, 1120).len(), 1, "no switch: the edge was dropped");
        assert_eq!((o.active(), o.active_c()), (Target::Mac, Target::Mac));
    }

    /// Rule C needs both sides: Windows activity alone, or an entry without a Mac
    /// edge, or a Mac edge without an entry, never switches.
    #[test]
    fn rule_c_needs_the_mac_edge_and_the_windows_entry() {
        let mut o = Observer::new(FlowConfig::default(), 2);
        o.on_win_activity(500);
        assert_eq!(o.on_win_edge(WinEdgeEvent::Departed, 600).len(), 1, "entry without a Mac edge");
        o.on_pointer(PointerEvent::EdgeArrived(0), 5000, Some(0.0));
        o.on_win_activity(5100);
        assert_eq!(o.active_c(), Target::Mac, "an edge with activity but no entry");
        o.on_pointer(PointerEvent::EdgeArrived(1), 9000, Some(0.0));
        assert_eq!(o.on_win_edge(WinEdgeEvent::Entered, 10_600).len(), 1, "entry after the pairing window");
        assert_eq!(o.active_c(), Target::Mac);
    }

    /// A slow hand-over with a small nudge in between, and an entry reported just
    /// before the Mac edge is sampled: rule C still pairs them.
    #[test]
    fn rule_c_pairs_slow_and_early_entries() {
        let mut o = Observer::new(FlowConfig::default(), 2);
        o.on_pointer(PointerEvent::EdgeArrived(0), 1000, Some(0.0));
        o.on_pointer(PointerEvent::Moved(Some(0)), 1200, Some(6.0));
        assert!(o.on_win_edge(WinEdgeEvent::Entered, 1900).contains(&Note::ToWindows { rule: Rule::C, display: Some(0), after_edge_ms: Some(900) }));
        let mut o = Observer::new(FlowConfig::default(), 2);
        o.on_win_edge(WinEdgeEvent::Entered, 2000);
        assert!(o.on_pointer(PointerEvent::EdgeArrived(1), 2100, Some(0.0)).contains(&Note::ToWindows { rule: Rule::C, display: Some(1), after_edge_ms: Some(0) }));
    }

    /// Observed on 2026-10-31: Mac moves 150-500 ms after a switch. Rule C keeps
    /// Windows while Windows is active and no exit was seen; it returns on an exit.
    #[test]
    fn rule_c_holds_mac_moves_until_an_exit_or_a_quiet_windows() {
        let mut o = Observer::new(FlowConfig::default(), 2);
        o.on_pointer(PointerEvent::EdgeArrived(1), 1000, Some(0.0));
        o.on_win_edge(WinEdgeEvent::Entered, 1050);
        o.on_win_activity(1100);
        let n = o.on_pointer(PointerEvent::Moved(Some(1)), 1300, Some(120.0));
        assert!(n.contains(&Note::MacMoveHeld { after_windows_ms: 250, from_edge_pt: Some(120) }), "{n:?}");
        assert_eq!(o.active_c(), Target::Windows);
        o.on_win_activity(3000);
        o.on_win_edge(WinEdgeEvent::Reached, 3100);
        assert!(o.on_pointer(PointerEvent::Moved(Some(0)), 3300, Some(5.0)).contains(&Note::ToMac { rule: Rule::C, display: Some(0), after_windows_ms: Some(2250), from_edge_pt: Some(5) }));
        // No exit seen, but Windows quiet and the Mac pointer far away: also back (e.g. the trackpad).
        let mut o = Observer::new(FlowConfig::default(), 2);
        o.on_pointer(PointerEvent::EdgeArrived(0), 1000, Some(0.0));
        o.on_win_edge(WinEdgeEvent::Entered, 1050);
        o.on_win_activity(1100);
        assert_eq!(o.active_c(), Target::Windows);
        assert!(o.on_pointer(PointerEvent::Moved(Some(0)), 2200, Some(300.0)).iter().any(|n| matches!(n, Note::ToMac { rule: Rule::C, .. })));
    }

    #[test]
    fn entry_tracker_tells_a_jump_in_from_a_walk_out() {
        // Windows: one monitor; another to its right (that shared border is not an edge).
        let m = vec![Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 }, Rect { x: 1920.0, y: 0.0, w: 1920.0, h: 1080.0 }];
        let mut t = EntryTracker::default();
        assert_eq!(t.on_sample(&m, 900.0, 500.0), None, "baseline");
        assert_eq!(t.on_sample(&m, 0.0, 300.0), Some(WinEdgeEvent::Entered), "Flow puts the cursor on the left edge");
        assert_eq!(t.on_sample(&m, 0.0, 310.0), None);
        assert_eq!(t.on_sample(&m, 12.0, 310.0), Some(WinEdgeEvent::Departed));
        assert_eq!(t.on_sample(&m, 40.0, 310.0), None);
        assert_eq!(t.on_sample(&m, 0.0, 312.0), Some(WinEdgeEvent::Reached), "walked to the edge");
        assert_eq!(t.on_sample(&m, 1000.0, 100.0), Some(WinEdgeEvent::Departed));
        assert_eq!(t.on_sample(&m, 1920.0, 100.0), None, "the shared border is not the outer left edge");
    }

    #[test]
    fn entry_tracker_watches_the_configured_edge_and_displays() {
        // Receiver above the keyboard's host: it watches its bottom edge. Two monitors
        // side by side, not aligned (the second is 200 px lower and smaller).
        let m = vec![Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 }, Rect { x: 1920.0, y: 200.0, w: 1280.0, h: 720.0 }];
        for (edge, (x0, y0), (x1, y1)) in [
            (Edge::Bottom, (900.0, 300.0), (600.0, 1079.0)),
            (Edge::Top, (900.0, 600.0), (600.0, 0.0)),
            (Edge::Right, (2500.0, 500.0), (3199.0, 500.0)),
            (Edge::Left, (900.0, 500.0), (0.0, 500.0)),
        ] {
            let mut t = EntryTracker::facing(edge, None);
            assert_eq!(t.on_sample(&m, x0, y0), None, "{edge:?} baseline");
            assert_eq!(t.on_sample(&m, x1, y1), Some(WinEdgeEvent::Entered), "{edge:?} entry");
        }
        // The lower monitor's top edge is outer only where nothing is above it (x >= 1920).
        let mut t = EntryTracker::facing(Edge::Top, Some(vec![1]));
        t.on_sample(&m, 2500.0, 600.0);
        assert_eq!(t.on_sample(&m, 2500.0, 200.0), Some(WinEdgeEvent::Entered), "the stepped edge of monitor 2 counts");
        let mut t = EntryTracker::facing(Edge::Top, Some(vec![1]));
        t.on_sample(&m, 900.0, 600.0);
        assert_eq!(t.on_sample(&m, 900.0, 0.0), None, "monitor 1 is not a listed display");
        // The shared vertical border is never an outer edge.
        let mut t = EntryTracker::facing(Edge::Right, None);
        t.on_sample(&m, 900.0, 500.0);
        assert_eq!(t.on_sample(&m, 1919.0, 500.0), None);
    }

    #[test]
    fn rule_a_edges_without_windows_activity_expire_once() {
        let mut o = Observer::new(FlowConfig::default(), 2);
        o.on_pointer(PointerEvent::EdgeArrived(1), 1000, Some(0.0));
        assert_eq!(o.tick(1300), vec![]);
        assert_eq!(o.tick(1401), vec![Note::EdgeExpired { display: 1 }]);
        assert_eq!(o.tick(2000), vec![]);
        assert_eq!(o.counts.edges_expired, 1);
    }

    #[test]
    fn tracker_reports_the_distance_from_the_edge_arrival() {
        let (d, c) = (two(), [0, 1]);
        let mut t = PointerTracker::default();
        t.on_sample(&d, &c, Edge::Right, 100.0, 100.0);
        assert_eq!(t.from_edge_pt(), None);
        t.on_sample(&d, &c, Edge::Right, 1511.0, 100.0);
        t.on_sample(&d, &c, Edge::Right, 1508.0, 104.0);
        assert_eq!(t.from_edge_pt(), Some(5.0));
    }

    /// Observed on 2026-11-01 (+7.115 / +7.121): the Mac move arrives a few ms
    /// before Windows' `Reached`. Rule C returns at the `Reached`, not at the next move.
    #[test]
    fn rule_c_returns_when_reached_trails_a_held_mac_move() {
        let mut o = Observer::new(FlowConfig::default(), 2);
        o.on_pointer(PointerEvent::EdgeArrived(1), 1000, Some(0.0));
        o.on_win_edge(WinEdgeEvent::Departed, 1020);
        o.on_win_activity(6900);
        let n = o.on_pointer(PointerEvent::Moved(Some(0)), 7000, Some(429.0));
        assert!(n.contains(&Note::MacMoveHeld { after_windows_ms: 5980, from_edge_pt: Some(429) }), "{n:?}");
        let n = o.on_win_edge(WinEdgeEvent::Reached, 7006);
        assert!(n.contains(&Note::ToMac { rule: Rule::C, display: Some(0), after_windows_ms: Some(5986), from_edge_pt: Some(429) }), "{n:?}");
        assert_eq!(o.active_c(), Target::Mac);
        // Too late to pair: the held move is older than late_exit_ms.
        let mut o = Observer::new(FlowConfig::default(), 2);
        o.on_pointer(PointerEvent::EdgeArrived(1), 1000, Some(0.0));
        o.on_win_edge(WinEdgeEvent::Departed, 1020);
        o.on_win_activity(6900);
        o.on_pointer(PointerEvent::Moved(Some(0)), 7000, Some(429.0));
        o.on_win_activity(7200);
        assert_eq!(o.on_win_edge(WinEdgeEvent::Reached, 7400).len(), 1);
        assert_eq!(o.active_c(), Target::Windows);
    }

    #[test]
    fn rule_e_follows_the_mx4_only_when_it_arrives_or_leaves() {
        assert_eq!(rule_e(0, 0, true), None, "connected all along: no decision");
        assert_eq!(rule_e(0, 0, false), None);
        assert_eq!(rule_e(1, 0, true), Some(Target::Windows));
        assert_eq!(rule_e(0, 1, false), Some(Target::Mac), "to the Mac or a third host: the Mac");
        assert_eq!(rule_e(2, 1, true), Some(Target::Windows), "several between reports: where it is now");
        assert_eq!(rule_e(1, 1, false), Some(Target::Mac));
    }
}
