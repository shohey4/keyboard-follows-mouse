//! Key-state sync over a lossy, reordering datagram link (pure; time is passed in).
//!
//! The sender numbers every key event, keeps the last few for redundancy and
//! always attaches the full held-key snapshot. The receiver applies new events
//! in order, then reconciles to the snapshot, and releases everything when the
//! link goes quiet. Lost datagrams therefore cost at most one heartbeat of
//! staleness, and a lost key-up can never leave a key stuck.

use std::collections::{BTreeSet, VecDeque};

use crate::keys::WinKey;
use crate::protocol::{KeyEvent, KeysPayload, MAX_EVENTS, MAX_HELD};

pub const HISTORY: usize = 16;

#[derive(Default)]
pub struct KeySender { seq: u32, history: VecDeque<KeyEvent>, held: BTreeSet<u16> }

impl KeySender {
    pub fn push(&mut self, key: WinKey, down: bool, repeat: bool) -> KeysPayload {
        self.seq += 1;
        let k = key.wire();
        if down {
            // More than MAX_HELD distinct keys is not a real chord; ignore the extra.
            if !self.held.contains(&k) && self.held.len() >= MAX_HELD { self.seq -= 1; return self.heartbeat(); }
            self.held.insert(k);
        } else {
            self.held.remove(&k);
        }
        self.history.push_back(KeyEvent { seq: self.seq, key: k, down, repeat });
        if self.history.len() > HISTORY { self.history.pop_front(); }
        self.heartbeat()
    }

    /// The current state without new events; also sent every heartbeat interval.
    pub fn heartbeat(&self) -> KeysPayload {
        KeysPayload { last_seq: self.seq, events: self.history.iter().copied().collect(), held: self.held.iter().copied().collect() }
    }
}

const _: () = assert!(HISTORY <= MAX_EVENTS);

/// Windows-side effect sink (SendInput in the real receiver).
pub trait Injector { fn inject(&mut self, key: WinKey, down: bool); }

pub struct KeyReceiver {
    applied: u32,
    held: BTreeSet<WinKey>,
    last_rx_ms: Option<u64>,
    timeout_ms: u64,
    /// Events that fell outside the redundancy window (repaired by the snapshot).
    pub gaps: u64,
}

impl KeyReceiver {
    pub fn new(timeout_ms: u64) -> Self {
        KeyReceiver { applied: 0, held: BTreeSet::new(), last_rx_ms: None, timeout_ms, gaps: 0 }
    }

    pub fn held(&self) -> impl Iterator<Item = &WinKey> { self.held.iter() }
    pub fn applied(&self) -> u32 { self.applied }

    pub fn on_payload(&mut self, p: &KeysPayload, now_ms: u64, inj: &mut impl Injector) {
        self.last_rx_ms = Some(now_ms);
        if p.last_seq < self.applied { return; } // older state than we already have
        let mut fresh: Vec<&KeyEvent> = p.events.iter().filter(|e| e.seq > self.applied).collect();
        fresh.sort_by_key(|e| e.seq);
        if let Some(first) = fresh.first() { if first.seq > self.applied + 1 { self.gaps += 1; } }
        else if p.last_seq > self.applied { self.gaps += 1; }
        for e in fresh {
            let Some(k) = WinKey::from_wire(e.key) else { continue };
            if e.down {
                if self.held.insert(k) || e.repeat { inj.inject(k, true); }
            } else if self.held.remove(&k) {
                inj.inject(k, false);
            }
        }
        let snapshot: BTreeSet<WinKey> = p.held.iter().filter_map(|w| WinKey::from_wire(*w)).collect();
        let stale: Vec<WinKey> = self.held.difference(&snapshot).copied().collect();
        self.release(stale, inj);
        for k in snapshot.difference(&self.held).copied().collect::<Vec<_>>() {
            self.held.insert(k);
            inj.inject(k, true);
        }
        self.applied = p.last_seq;
    }

    /// Call periodically. Link silent for `timeout_ms` -> release every key.
    pub fn tick(&mut self, now_ms: u64, inj: &mut impl Injector) {
        if let Some(t) = self.last_rx_ms
            && now_ms.saturating_sub(t) >= self.timeout_ms
        {
            self.release_all(inj);
            self.last_rx_ms = None;
        }
    }

    /// New sender session (Mac restarted): its numbering starts over.
    pub fn on_new_session(&mut self, inj: &mut impl Injector) {
        self.release_all(inj);
        self.applied = 0;
    }

    pub fn release_all(&mut self, inj: &mut impl Injector) {
        let all: Vec<WinKey> = self.held.iter().copied().collect();
        self.release(all, inj);
    }

    fn release(&mut self, mut keys: Vec<WinKey>, inj: &mut impl Injector) {
        keys.sort_by_key(|k| (k.is_modifier(), *k));
        for k in keys {
            self.held.remove(&k);
            inj.inject(k, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{hid, win_key_for_usage, Usage};

    #[derive(Default)]
    struct Log(Vec<(WinKey, bool)>);
    impl Injector for Log { fn inject(&mut self, key: WinKey, down: bool) { self.0.push((key, down)); } }

    fn w(u: Usage) -> WinKey { win_key_for_usage(u).unwrap() }

    #[test]
    fn in_order_delivery_injects_exactly_once() {
        let (mut s, mut r, mut log) = (KeySender::default(), KeyReceiver::new(1000), Log::default());
        for (u, d) in [(hid::LSHIFT, true), (hid::A, true), (hid::A, false), (hid::LSHIFT, false)] {
            r.on_payload(&s.push(w(u), d, false), 0, &mut log);
        }
        assert_eq!(log.0, vec![(w(hid::LSHIFT), true), (w(hid::A), true), (w(hid::A), false), (w(hid::LSHIFT), false)]);
        r.on_payload(&s.heartbeat(), 10, &mut log); // duplicates change nothing
        assert_eq!(log.0.len(), 4);
        assert_eq!(r.gaps, 0);
    }

    #[test]
    fn lost_datagrams_recovered_from_history() {
        let (mut s, mut r, mut log) = (KeySender::default(), KeyReceiver::new(1000), Log::default());
        s.push(w(hid::A), true, false); // lost
        s.push(w(hid::A), false, false); // lost
        r.on_payload(&s.push(w(hid::C), true, false), 0, &mut log);
        assert_eq!(log.0, vec![(w(hid::A), true), (w(hid::A), false), (w(hid::C), true)]); // the "a" was still typed
        assert_eq!(r.gaps, 0);
    }

    #[test]
    fn beyond_history_the_snapshot_prevents_stuck_keys() {
        let (mut s, mut r, mut log) = (KeySender::default(), KeyReceiver::new(1000), Log::default());
        r.on_payload(&s.push(w(hid::LCTRL), true, false), 0, &mut log);
        s.push(w(hid::LCTRL), false, false); // lost...
        for _ in 0..HISTORY { s.push(w(hid::A), true, false); s.push(w(hid::A), false, false); } // ...and pushed out
        r.on_payload(&s.heartbeat(), 5, &mut log);
        assert_eq!(r.held().count(), 0, "Ctrl must not stay down");
        assert_eq!(r.gaps, 1);
    }

    #[test]
    fn stale_payload_ignored() {
        let (mut s, mut r, mut log) = (KeySender::default(), KeyReceiver::new(1000), Log::default());
        let old = s.push(w(hid::A), true, false);
        r.on_payload(&s.push(w(hid::A), false, false), 0, &mut log);
        let n = log.0.len();
        r.on_payload(&old, 1, &mut log);
        assert_eq!(log.0.len(), n);
        assert_eq!(r.held().count(), 0);
    }

    #[test]
    fn autorepeat_is_injected_again() {
        let (mut s, mut r, mut log) = (KeySender::default(), KeyReceiver::new(1000), Log::default());
        r.on_payload(&s.push(w(hid::A), true, false), 0, &mut log);
        r.on_payload(&s.push(w(hid::A), true, true), 30, &mut log);
        assert_eq!(log.0, vec![(w(hid::A), true), (w(hid::A), true)]);
    }

    #[test]
    fn watchdog_releases_everything_modifiers_last() {
        let (mut s, mut r, mut log) = (KeySender::default(), KeyReceiver::new(500), Log::default());
        r.on_payload(&s.push(w(hid::LCTRL), true, false), 0, &mut log);
        r.on_payload(&s.push(w(hid::C), true, false), 10, &mut log);
        r.tick(400, &mut log);
        assert_eq!(r.held().count(), 2);
        r.tick(510, &mut log);
        assert_eq!(&log.0[2..], &[(w(hid::C), false), (w(hid::LCTRL), false)]);
        r.tick(2000, &mut log);
        assert_eq!(log.0.len(), 4); // released once
    }

    #[test]
    fn new_session_releases_and_restarts_numbering() {
        let (mut s, mut r, mut log) = (KeySender::default(), KeyReceiver::new(1000), Log::default());
        for _ in 0..5 { s.push(w(hid::A), true, false); s.push(w(hid::A), false, false); }
        r.on_payload(&s.push(w(hid::LSHIFT), true, false), 0, &mut log);
        r.on_new_session(&mut log);
        assert_eq!(r.held().count(), 0);
        let mut s2 = KeySender::default();
        r.on_payload(&s2.push(w(hid::C), true, false), 1, &mut log);
        assert_eq!(log.0.last(), Some(&(w(hid::C), true)));
    }

    /// Deterministic lossy, reordering link: whatever is dropped or swapped,
    /// once a later datagram arrives the receiver holds exactly what the sender holds.
    #[test]
    fn converges_under_random_loss_and_reorder() {
        let keys = [hid::A, hid::C, hid::V, hid::LSHIFT, hid::LCTRL, hid::LGUI];
        let mut rng: u64 = 0x9E3779B97F4A7C15;
        let mut next = || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };
        for _round in 0..200 {
            let (mut s, mut r, mut log) = (KeySender::default(), KeyReceiver::new(10_000), Log::default());
            let mut in_flight: Vec<KeysPayload> = Vec::new();
            for t in 0..60u64 {
                let k = w(keys[(next() % keys.len() as u64) as usize]);
                let down = next() % 2 == 0;
                in_flight.push(s.push(k, down, false));
                if next() % 3 == 0 && in_flight.len() > 1 { let n = in_flight.len(); in_flight.swap(n - 1, n - 2); }
                if next() % 2 == 0 {
                    for p in in_flight.drain(..) { if next() % 3 != 0 { r.on_payload(&p, t, &mut log); } }
                }
            }
            r.on_payload(&s.heartbeat(), 100, &mut log);
            let want: BTreeSet<WinKey> = s.heartbeat().held.iter().map(|x| WinKey::from_wire(*x).unwrap()).collect();
            assert_eq!(r.held().copied().collect::<BTreeSet<_>>(), want);
            // The injected stream itself is consistent: no up without a prior down.
            let mut down = BTreeSet::new();
            for (k, d) in &log.0 { if *d { down.insert(*k); } else { assert!(down.remove(k), "up without down"); } }
        }
    }
}
