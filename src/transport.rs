//! Link state machines on top of `protocol` + `link` (pure; the binaries own
//! the UDP sockets and the clock).
//!
//! Mac side (`MacLink`): learns the receiver's challenge from an authenticated
//! Ack, seals key payloads, and decides whether the link is up: an Ack that
//! echoes our current session within `timeout_ms`, and the receiver must have
//! applied what we sent more than `timeout_ms` ago (so a one-way loss
//! Mac->Windows is "down" too). An Ack that does not echo our session (a hello
//! from a receiver that has not heard this session yet, or a replay) can only
//! tell us which challenge to use, and only while the link is not live.
//! Windows side (`WinLink`): accepts only frames from the pinned peer address
//! (checked by the binary) that authenticate under the current challenge,
//! applies them, acks, and releases everything when the link goes quiet.

use crate::flow::WinEdgeEvent;
use crate::link::{Injector, KeyReceiver, KeySender};
use crate::keys::WinKey;
use crate::protocol::{random16, AckPayload, Challenge, Direction, FrameError, KeysPayload, Opener, Sealer, SessionId};

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Mac heartbeat and Windows ack/hello interval.
    pub heartbeat_ms: u64,
    /// Mac: no fresh Ack (or nothing applied) for this long -> link down.
    pub link_timeout_ms: u64,
    /// Windows: nothing authenticated for this long -> release every key.
    pub watchdog_ms: u64,
}

impl Default for Timing {
    fn default() -> Self { Timing { heartbeat_ms: 50, link_timeout_ms: 300, watchdog_ms: 500 } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkChange { Up, Down }

pub struct MacLink {
    key: [u8; 32],
    timing: Timing,
    sealer: Sealer,
    opener: Opener,
    challenge: Option<Challenge>,
    sender: KeySender,
    last_ack_ms: Option<u64>,
    acked_seq: u32,
    /// When the oldest not-yet-applied event was sent.
    unapplied_since: Option<u64>,
    last_tx_ms: Option<u64>,
    up: bool,
    /// Receiver counts already reported by `take_peer`, and the latest ones.
    peer_seen: Option<PeerCounts>,
    peer: Option<PeerCounts>,
    /// Receiver-reported time since its last mouse move, as of the last Ack.
    pub peer_mouse_idle_ms: Option<u32>,
    /// The (receiver session, counter) of the last verified Ack: only newer
    /// ones count, even if the opener's per-session history was evicted.
    verified: Option<([u8; 16], u64)>,
    pub rejected: u64,
}

impl MacLink {
    pub fn new(key: [u8; 32], timing: Timing) -> Self {
        MacLink {
            key, timing,
            sealer: Sealer::new(&key, Direction::MacToWin, random16()),
            opener: Opener::new(&key, Direction::WinToMac, None),
            challenge: None, sender: KeySender::default(), last_ack_ms: None, acked_seq: 0,
            unapplied_since: None, last_tx_ms: None, up: false, peer_seen: None, peer: None, peer_mouse_idle_ms: None, verified: None, rejected: 0,
        }
    }

    pub fn is_up(&self) -> bool { self.up }

    /// Diagnosis for a link change: how long since the last Ack, and how long
    /// sent keys have waited to be applied (`None` = nothing waiting).
    pub fn ages(&self, now: u64) -> (Option<u64>, Option<u64>) {
        (self.last_ack_ms.map(|a| now.saturating_sub(a)), self.unapplied_since.map(|s| now.saturating_sub(s)))
    }

    /// A key event from the router. Returns the frame to send (None until the
    /// receiver's challenge is known — the router refuses Windows until then).
    pub fn key(&mut self, key: WinKey, down: bool, repeat: bool, now: u64) -> Option<Vec<u8>> {
        let payload = self.sender.push(key, down, repeat);
        if self.unapplied_since.is_none() { self.unapplied_since = Some(now); }
        self.seal(&payload, now)
    }

    /// Authenticated Ack from the receiver.
    pub fn on_datagram(&mut self, frame: &[u8], now: u64) -> Option<LinkChange> {
        let opened = match self.opener.open(frame) { Ok(o) => o, Err(_) => { self.rejected += 1; return None; } };
        let Ok(ack) = AckPayload::decode(&opened.plaintext) else { self.rejected += 1; return None; };
        if self.challenge != Some(opened.challenge) || ack.echo != self.sealer.session() {
            // Not an answer to this session. Its only use is the challenge, and
            // only while no verified Ack is fresh: a live link ignores it, so a
            // replayed or foreign hello cannot reset the session under us.
            if self.challenge != Some(opened.challenge) && !self.fresh(now) {
                // Receiver (re)started, or we did: start a fresh session under
                // its challenge; it releases everything when that session arrives.
                self.challenge = Some(opened.challenge);
                self.sealer = Sealer::new(&self.key, Direction::MacToWin, random16());
                self.sender = KeySender::default();
                self.acked_seq = 0;
                self.unapplied_since = None;
                self.last_ack_ms = None;
                // The first verified Ack of the new session sets the baseline counts.
                self.peer_seen = None;
                self.peer = None;
                self.peer_mouse_idle_ms = None;
                self.verified = None;
            }
            return self.evaluate(now);
        }
        if self.verified.is_some_and(|(s, c)| s == opened.session && opened.counter <= c) { self.rejected += 1; return None; }
        self.verified = Some((opened.session, opened.counter));
        if ack.last_applied > self.acked_seq { self.acked_seq = ack.last_applied; }
        if self.acked_seq >= self.sender.heartbeat().last_seq { self.unapplied_since = None; }
        self.peer = Some(PeerCounts::of(&ack));
        self.peer_mouse_idle_ms = (ack.mouse_idle_ms != u32::MAX).then_some(ack.mouse_idle_ms);
        if self.peer_seen.is_none() { self.peer_seen = self.peer; }
        self.last_ack_ms = Some(now);
        self.evaluate(now)
    }

    /// What the receiver reported since the last call: whether its mouse moved,
    /// how many of each left-edge event, and the MX4's arrivals / departures
    /// (with whether it is there now). Stale/replayed Acks never get here.
    pub fn take_peer(&mut self) -> PeerDelta {
        let (Some(now), Some(seen)) = (self.peer, self.peer_seen) else { return PeerDelta::default() };
        self.peer_seen = Some(now);
        PeerDelta {
            moved: now.moves != seen.moves,
            entered: now.entered.wrapping_sub(seen.entered),
            departed: now.departed.wrapping_sub(seen.departed),
            reached: now.reached.wrapping_sub(seen.reached),
            mx4_arrivals: now.mx4_arrivals.wrapping_sub(seen.mx4_arrivals),
            mx4_departures: now.mx4_departures.wrapping_sub(seen.mx4_departures),
            mx4_present: now.mx4_present,
        }
    }
    pub fn take_peer_mouse(&mut self) -> bool { self.take_peer().moved }

    /// Call often. Returns a heartbeat frame when due and any link change.
    pub fn tick(&mut self, now: u64) -> (Option<Vec<u8>>, Option<LinkChange>) {
        let due = self.last_tx_ms.is_none_or(|t| now.saturating_sub(t) >= self.timing.heartbeat_ms);
        let frame = if due { let hb = self.sender.heartbeat(); self.seal(&hb, now) } else { None };
        (frame, self.evaluate(now))
    }

    fn seal(&mut self, payload: &KeysPayload, now: u64) -> Option<Vec<u8>> {
        let challenge = self.challenge?;
        self.last_tx_ms = Some(now);
        Some(self.sealer.seal(&challenge, &payload.encode()))
    }

    /// A verified Ack (one that echoed our session) within the link timeout.
    fn fresh(&self, now: u64) -> bool {
        self.last_ack_ms.is_some_and(|a| now.saturating_sub(a) < self.timing.link_timeout_ms)
    }

    fn evaluate(&mut self, now: u64) -> Option<LinkChange> {
        let t = self.timing.link_timeout_ms;
        let fresh = self.fresh(now);
        let applying = self.unapplied_since.is_none_or(|s| now.saturating_sub(s) < t);
        let up = self.challenge.is_some() && fresh && applying;
        if up == self.up { return None; }
        self.up = up;
        Some(if up { LinkChange::Up } else { LinkChange::Down })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerCounts { moves: u32, entered: u32, departed: u32, reached: u32, mx4_present: bool, mx4_arrivals: u32, mx4_departures: u32 }
impl PeerCounts {
    fn of(a: &AckPayload) -> Self {
        PeerCounts { moves: a.mouse_moves, entered: a.entered, departed: a.departed, reached: a.reached,
            mx4_present: a.mx4_present, mx4_arrivals: a.mx4_arrivals, mx4_departures: a.mx4_departures }
    }
}

/// Receiver reports since the previous `MacLink::take_peer`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerDelta { pub moved: bool, pub entered: u32, pub departed: u32, pub reached: u32, pub mx4_arrivals: u32, pub mx4_departures: u32, pub mx4_present: bool }

pub struct WinLink {
    timing: Timing,
    challenge: Challenge,
    opener: Opener,
    sealer: Sealer,
    receiver: KeyReceiver,
    last_ack_ms: Option<u64>,
    mouse_moves: u32,
    mouse_last_ms: Option<u64>,
    /// Left-edge events: entered, departed, reached.
    edges: [u32; 3],
    /// MX4 in this PC's device list (None until the first sample), and its
    /// arrivals / departures since.
    mx4: Option<bool>,
    mx4_counts: [u32; 2],
    /// The Mac session of the last accepted frame, echoed in every Ack.
    mac_session: SessionId,
    pub rejected: u64,
}

impl WinLink {
    pub fn new(key: [u8; 32], timing: Timing) -> Self {
        let challenge = random16();
        WinLink {
            timing, challenge,
            opener: Opener::new(&key, Direction::MacToWin, Some(challenge)),
            sealer: Sealer::new(&key, Direction::WinToMac, random16()),
            receiver: KeyReceiver::new(timing.watchdog_ms),
            last_ack_ms: None, mouse_moves: 0, mouse_last_ms: None, edges: [0; 3], mx4: None, mx4_counts: [0; 2], mac_session: [0; 16], rejected: 0,
        }
    }

    pub fn held(&self) -> usize { self.receiver.held().count() }
    pub fn mouse_moves(&self) -> u32 { self.mouse_moves }

    /// The binary saw this PC's cursor move (a poll, not every pixel).
    pub fn mouse_moved(&mut self, now: u64) {
        self.mouse_moves = self.mouse_moves.wrapping_add(1);
        self.mouse_last_ms = Some(now);
    }

    pub fn edge_counts(&self) -> [u32; 3] { self.edges }

    /// The binary saw this left-edge event; send `ack_now` so the Mac learns it at once.
    pub fn edge_event(&mut self, e: WinEdgeEvent) {
        let i = match e { WinEdgeEvent::Entered => 0, WinEdgeEvent::Departed => 1, WinEdgeEvent::Reached => 2 };
        self.edges[i] = self.edges[i].wrapping_add(1);
    }

    pub fn mx4_counts(&self) -> [u32; 2] { self.mx4_counts }
    pub fn mx4_present(&self) -> bool { self.mx4 == Some(true) }

    /// One device-list sample: is the MX4 there? The first sample only sets the
    /// state (no arrival is counted for a mouse already connected at start).
    /// Returns true when it changed; then send `ack_now`.
    pub fn mx4_sample(&mut self, present: bool) -> bool {
        let before = self.mx4.replace(present);
        if before.is_none_or(|b| b == present) { return false; }
        let i = if present { 0 } else { 1 };
        self.mx4_counts[i] = self.mx4_counts[i].wrapping_add(1);
        true
    }

    /// An Ack now (not waiting for the heartbeat), e.g. right after `edge_event`.
    pub fn ack_now(&mut self, now: u64) -> Vec<u8> { self.ack(now) }

    /// A datagram from the pinned peer. Returns the Ack to send back.
    pub fn on_datagram(&mut self, frame: &[u8], now: u64, inj: &mut impl Injector) -> Result<Vec<u8>, FrameError> {
        let opened = self.opener.open(frame).inspect_err(|_| self.rejected += 1)?;
        let payload = KeysPayload::decode(&opened.plaintext).inspect_err(|_| self.rejected += 1)?;
        if opened.new_session { self.receiver.on_new_session(inj); }
        self.mac_session = opened.session;
        self.receiver.on_payload(&payload, now, inj);
        Ok(self.ack(now))
    }

    /// Call often: watchdog, and a periodic Ack/hello so a (re)starting Mac
    /// learns the challenge.
    pub fn tick(&mut self, now: u64, inj: &mut impl Injector) -> Option<Vec<u8>> {
        self.receiver.tick(now, inj);
        let due = self.last_ack_ms.is_none_or(|t| now.saturating_sub(t) >= self.timing.heartbeat_ms);
        due.then(|| self.ack(now))
    }

    /// Process exit / explicit stop.
    pub fn release_all(&mut self, inj: &mut impl Injector) { self.receiver.release_all(inj); }

    fn ack(&mut self, now: u64) -> Vec<u8> {
        self.last_ack_ms = Some(now);
        let mouse_idle_ms = self.mouse_last_ms.map_or(u32::MAX, |t| now.saturating_sub(t).min(u64::from(u32::MAX - 1)) as u32);
        let [entered, departed, reached] = self.edges;
        let [mx4_arrivals, mx4_departures] = self.mx4_counts;
        let ack = AckPayload { last_applied: self.receiver.applied(), mouse_moves: self.mouse_moves, mouse_idle_ms, entered, departed, reached,
            mx4_present: self.mx4_present(), mx4_arrivals, mx4_departures, echo: self.mac_session };
        self.sealer.seal(&self.challenge, &ack.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{hid, win_key_for_usage, Usage};

    #[derive(Default)]
    struct Log(Vec<(WinKey, bool)>);
    impl Injector for Log { fn inject(&mut self, key: WinKey, down: bool) { self.0.push((key, down)); } }

    const KEY: [u8; 32] = [42; 32];
    fn w(u: Usage) -> WinKey { win_key_for_usage(u).unwrap() }

    /// Mac learns the challenge from a hello, and comes up only when an Ack
    /// echoes its session (after the receiver accepted a frame of it).
    fn connected(now: u64) -> (MacLink, WinLink, Log) {
        let (mut m, mut win, mut log) = (MacLink::new(KEY, Timing::default()), WinLink::new(KEY, Timing::default()), Log::default());
        assert!(m.key(w(hid::A), true, false, now).is_none(), "nothing is sent before the challenge is known");
        let hello = win.tick(now, &mut log).unwrap();
        assert_eq!(m.on_datagram(&hello, now), None, "a hello alone does not bring the link up");
        let (hb, _) = m.tick(now);
        let ack = win.on_datagram(&hb.unwrap(), now, &mut log).unwrap();
        assert_eq!(m.on_datagram(&ack, now), Some(LinkChange::Up));
        (m, win, log)
    }

    #[test]
    fn end_to_end_typing() {
        let (mut m, mut win, mut log) = connected(0);
        for (i, (u, d)) in [(hid::LSHIFT, true), (hid::A, true), (hid::A, false), (hid::LSHIFT, false)].into_iter().enumerate() {
            let f = m.key(w(u), d, false, i as u64).unwrap();
            let ack = win.on_datagram(&f, i as u64, &mut log).unwrap();
            assert_eq!(m.on_datagram(&ack, i as u64), None);
        }
        // The earlier unsent "a down" (pre-challenge) belonged to the old session and was discarded.
        assert_eq!(log.0, vec![(w(hid::LSHIFT), true), (w(hid::A), true), (w(hid::A), false), (w(hid::LSHIFT), false)]);
        assert!(m.is_up());
    }

    #[test]
    fn corrupted_and_foreign_frames_change_nothing() {
        let (mut m, mut win, mut log) = connected(0);
        let mut f = m.key(w(hid::A), true, false, 1).unwrap();
        let last = f.len() - 1;
        f[last] ^= 0x55;
        assert_eq!(win.on_datagram(&f, 1, &mut log).err(), Some(FrameError::Auth));
        let mut other = MacLink::new([1; 32], Timing::default());
        other.challenge = Some(win.challenge); // even knowing the challenge, a wrong key fails
        let forged = other.key(w(hid::A), true, false, 1).unwrap();
        assert_eq!(win.on_datagram(&forged, 1, &mut log).err(), Some(FrameError::Auth));
        assert!(log.0.is_empty());
        assert_eq!(win.rejected, 2);
        assert_eq!(m.on_datagram(b"garbage", 1), None);
        assert_eq!(m.rejected, 1);
    }

    #[test]
    fn half_open_windows_to_mac_lost_goes_down_and_watchdog_releases() {
        let (mut m, mut win, mut log) = connected(0);
        let f = m.key(w(hid::LCTRL), true, false, 10).unwrap();
        win.on_datagram(&f, 10, &mut log).unwrap(); // Ack is lost
        let (_, change) = m.tick(400);
        assert_eq!(change, Some(LinkChange::Down));
        assert_eq!(m.ages(400), (Some(400), Some(390)), "diagnosis: Ack age and how long the key has waited");
        // Mac stops sending heartbeats? No: it keeps sending; but if they are
        // lost too, the receiver's watchdog releases Ctrl.
        win.tick(520, &mut log);
        assert_eq!(log.0.last(), Some(&(w(hid::LCTRL), false)));
        assert_eq!(win.held(), 0);
    }

    #[test]
    fn half_open_mac_to_windows_lost_is_detected_by_unapplied_events() {
        let (mut m, mut win, mut log) = connected(0);
        m.key(w(hid::A), true, false, 10).unwrap(); // lost on the way to Windows
        // Windows keeps acking (hello), but its last_applied never reaches seq 1.
        let mut change = None;
        for t in (50..=400).step_by(50) {
            if let Some(hello) = win.tick(t, &mut log) { change = change.or(m.on_datagram(&hello, t)); }
        }
        assert_eq!(change, Some(LinkChange::Down));
        assert!(log.0.is_empty());
    }

    #[test]
    fn heartbeat_recovers_a_lost_key_up() {
        let (mut m, mut win, mut log) = connected(0);
        let down = m.key(w(hid::A), true, false, 1).unwrap();
        win.on_datagram(&down, 1, &mut log).unwrap();
        m.key(w(hid::A), false, false, 2).unwrap(); // the up is lost
        let (hb, _) = m.tick(60);
        win.on_datagram(&hb.unwrap(), 60, &mut log).unwrap();
        assert_eq!(log.0, vec![(w(hid::A), true), (w(hid::A), false)]);
    }

    #[test]
    fn receiver_restart_new_challenge_new_session_old_frames_dead() {
        let (mut m, mut win, mut log) = connected(0);
        let old = m.key(w(hid::A), true, false, 1).unwrap();
        win.on_datagram(&old, 1, &mut log).unwrap();
        let mut win2 = WinLink::new(KEY, Timing::default()); // Windows receiver restarted
        let mut log2 = Log::default();
        assert_eq!(win2.on_datagram(&old, 2, &mut log2).err(), Some(FrameError::WrongChallenge));
        // While a verified Ack is fresh, a hello with another challenge is ignored
        // (it could be a replay); the Mac keeps its session.
        let hello = win2.tick(3, &mut log2).unwrap();
        m.on_datagram(&hello, 3);
        let f = m.key(w(hid::V), true, false, 4).unwrap();
        assert_eq!(win2.on_datagram(&f, 4, &mut log2).err(), Some(FrameError::WrongChallenge));
        // Once the link has timed out, the next hello starts a session under the new challenge.
        let hello = win2.tick(400, &mut log2).unwrap();
        m.on_datagram(&hello, 400);
        let f = m.key(w(hid::C), true, false, 401).unwrap();
        let ack = win2.on_datagram(&f, 401, &mut log2).unwrap();
        assert_eq!(log2.0, vec![(w(hid::C), true)]);
        m.on_datagram(&ack, 401);
        assert!(m.is_up());
    }

    #[test]
    fn one_replayed_ack_from_an_old_receiver_session_does_not_lock_out_the_live_one() {
        // An Ack recorded from an earlier receiver run (same key).
        let (mut m0, mut old_win, mut log0) = connected(0);
        let f = m0.key(w(hid::A), true, false, 1).unwrap();
        let old_ack = old_win.on_datagram(&f, 1, &mut log0).unwrap();
        // A restarted Mac, up with the live receiver.
        let (mut m, mut win, mut log) = connected(100);
        assert_eq!(m.on_datagram(&old_ack, 110), None, "the replay is not trusted");
        assert!(m.is_up());
        for t in [150u64, 200, 250, 300, 350, 400, 450] {
            let (hb, _) = m.tick(t);
            if let Some(hb) = hb { let ack = win.on_datagram(&hb, t, &mut log).unwrap(); m.on_datagram(&ack, t); }
            assert!(m.is_up(), "the live session keeps the link up at {t}");
        }
        assert_eq!(m.on_datagram(&old_ack, 460), None);
        assert_eq!(m.rejected, 1, "a second copy of the same frame is a replay");
    }

    #[test]
    fn a_live_ack_replayed_after_flooding_old_sessions_is_still_refused() {
        let (mut m, mut win, mut log) = connected(0);
        let (hb, _) = m.tick(50);
        let early = win.on_datagram(&hb.unwrap(), 50, &mut log).unwrap();
        m.on_datagram(&early, 50);
        win.mx4_sample(false);
        win.mx4_sample(true); // an arrival, reported in the next Ack
        let (hb, _) = m.tick(100);
        let later = win.on_datagram(&hb.unwrap(), 100, &mut log).unwrap();
        m.on_datagram(&later, 100);
        assert_eq!(m.take_peer().mx4_arrivals, 1);
        // Hellos from 300 other receiver runs push the live session out of the opener's history.
        for i in 0..300u64 { let mut other = WinLink::new(KEY, Timing::default()); m.on_datagram(&other.tick(110 + i, &mut Log::default()).unwrap(), 110); }
        let before = m.rejected;
        assert_eq!(m.on_datagram(&early, 120), None);
        assert_eq!(m.rejected, before + 1, "older than the last verified Ack of this session");
        assert_eq!(m.take_peer(), PeerDelta { mx4_present: true, ..Default::default() }, "counts do not go backwards");
    }

    #[test]
    fn acks_replayed_after_a_mac_restart_do_not_fake_a_live_link() {
        // Record a live session's Acks (they report keys as applied).
        let (mut m, mut win, mut log) = connected(0);
        let mut recorded = Vec::new();
        for t in 1..=5u64 {
            let f = m.key(w(hid::A), t % 2 == 1, false, t).unwrap();
            recorded.push(win.on_datagram(&f, t, &mut log).unwrap());
        }
        // The Mac restarts; the receiver is gone (or never hears the new session).
        let mut m2 = MacLink::new(KEY, Timing::default());
        let mut up = false;
        for (i, ack) in recorded.iter().enumerate() {
            let now = 1000 + i as u64;
            up |= m2.on_datagram(ack, now) == Some(LinkChange::Up);
            if let Some(f) = m2.key(w(hid::V), true, false, now) { assert!(!f.is_empty()); }
        }
        let (_, change) = m2.tick(1010);
        assert!(!up && change.is_none() && !m2.is_up(), "replayed Acks never echo the new session");
        assert_eq!(m2.take_peer(), PeerDelta::default(), "and report nothing");
    }

    #[test]
    fn replayed_frame_is_rejected() {
        let (mut m, mut win, mut log) = connected(0);
        let f = m.key(w(hid::A), true, false, 1).unwrap();
        win.on_datagram(&f, 1, &mut log).unwrap();
        assert_eq!(win.on_datagram(&f, 2, &mut log).err(), Some(FrameError::Replay));
        assert_eq!(log.0.len(), 1);
    }

    #[test]
    fn receiver_mouse_activity_reaches_the_mac_once_per_change() {
        let (mut m, mut win, mut log) = connected(0);
        assert!(!m.take_peer_mouse(), "the first Ack is only the baseline");
        win.mouse_moved(10);
        win.mouse_moved(20);
        let ack = win.tick(100, &mut log).unwrap();
        m.on_datagram(&ack, 100);
        assert_eq!(m.peer_mouse_idle_ms, Some(80));
        assert!(m.take_peer_mouse());
        assert!(!m.take_peer_mouse(), "reported once");
        let ack = win.tick(200, &mut log).unwrap();
        m.on_datagram(&ack, 200);
        assert!(!m.take_peer_mouse(), "no new movement");
        // A restarted receiver counts from zero: that is a new baseline, not movement.
        // (Its hello is taken only after the old link has timed out.)
        let mut win2 = WinLink::new(KEY, Timing::default());
        let hello = win2.tick(600, &mut log).unwrap();
        m.on_datagram(&hello, 600);
        assert!(!m.take_peer_mouse());
        assert_eq!(m.peer_mouse_idle_ms, None);
        let (hb, _) = m.tick(650);
        let ack = win2.on_datagram(&hb.unwrap(), 650, &mut log).unwrap();
        m.on_datagram(&ack, 650);
        assert!(!m.take_peer_mouse(), "the first verified Ack of the new session is only the baseline");
    }

    #[test]
    fn receiver_edge_events_reach_the_mac_as_counts() {
        let (mut m, mut win, _) = connected(0);
        assert_eq!(m.take_peer(), PeerDelta::default());
        win.edge_event(WinEdgeEvent::Entered);
        win.edge_event(WinEdgeEvent::Departed);
        win.mouse_moved(5);
        let ack = win.ack_now(10);
        m.on_datagram(&ack, 10);
        assert_eq!(m.take_peer(), PeerDelta { moved: true, entered: 1, departed: 1, reached: 0, ..Default::default() });
        assert_eq!(m.take_peer(), PeerDelta::default(), "reported once");
        win.edge_event(WinEdgeEvent::Reached);
        m.on_datagram(&win.ack_now(20), 20);
        assert_eq!(m.take_peer(), PeerDelta { moved: false, entered: 0, departed: 0, reached: 1, ..Default::default() });
        assert_eq!(win.edge_counts(), [1, 1, 1]);
    }

    #[test]
    fn mx4_arrivals_and_departures_reach_the_mac_once() {
        let (mut m, mut win, _) = connected(0);
        assert!(!win.mx4_sample(true), "already connected at start: state only, no arrival");
        m.on_datagram(&win.ack_now(5), 5);
        assert_eq!(m.take_peer(), PeerDelta { mx4_present: true, ..Default::default() });
        assert!(!win.mx4_sample(true), "no change, nothing to report");
        assert!(win.mx4_sample(false));
        m.on_datagram(&win.ack_now(10), 10);
        assert_eq!(m.take_peer(), PeerDelta { mx4_departures: 1, ..Default::default() });
        assert!(win.mx4_sample(true));
        assert!(win.mx4_sample(false));
        assert!(win.mx4_sample(true));
        m.on_datagram(&win.ack_now(20), 20);
        assert_eq!(m.take_peer(), PeerDelta { mx4_arrivals: 2, mx4_departures: 1, mx4_present: true, ..Default::default() }, "several between Acks: counts plus where it is now");
        assert_eq!(m.take_peer(), PeerDelta { mx4_present: true, ..Default::default() }, "reported once");
        assert_eq!(win.mx4_counts(), [2, 2]);
    }
}
