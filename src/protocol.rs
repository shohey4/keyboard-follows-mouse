//! Wire format: authenticated, encrypted datagrams (never plaintext UDP).
//!
//! frame = header (44 bytes, authenticated as AAD) || XChaCha20-Poly1305 ciphertext
//! header = "KFM" | version | direction | session id (16) | counter (8, BE) | challenge (16)
//!
//! - Key: 32-byte pre-shared key from pairing (v0). Each side picks a random
//!   128-bit session id per run; nonce = session id || counter, so a restart
//!   never reuses a nonce under the same key.
//! - Replay: counters must strictly increase within a session; session ids
//!   already retired are refused; and every Mac->Windows frame must echo the
//!   receiver's current random challenge (sent in its authenticated Ack), so
//!   frames captured before a receiver restart are dead. In the other
//!   direction every Ack echoes the Mac session it last accepted; the Mac
//!   trusts only Acks that echo its current session, so Acks captured before a
//!   Mac restart cannot fake a live link.
//! - The direction byte is authenticated, so frames cannot be reflected.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::collections::VecDeque;

/// 2 = Ack carries the receiver's mouse activity (Flow follow, stage 7).
/// 3 = also its left-edge events (entered / departed / reached).
/// 4 = also whether the MX4 is connected to it, and running counts of its
///     arrivals / departures (Easy-Switch follow, rule E).
/// 5 = also the Mac session the receiver last accepted (`AckPayload::echo`).
/// A peer on another version is refused (BadVersion), so both sides are updated together.
pub const VERSION: u8 = 5;
const MAGIC: &[u8; 3] = b"KFM";
pub const HEADER_LEN: usize = 3 + 1 + 1 + 16 + 8 + 16;
const RETIRED_SESSIONS: usize = 64;
/// Mac side: how many Windows sessions keep their last counter.
const KNOWN_SESSIONS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction { MacToWin = 0, WinToMac = 1 }

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError { Short, BadMagic, BadVersion, WrongDirection, WrongChallenge, Auth, Replay, RetiredSession, Payload }

pub type SessionId = [u8; 16];
pub type Challenge = [u8; 16];

pub fn random16() -> [u8; 16] {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("OS random source");
    b
}

/// One sending side of one session.
pub struct Sealer { aead: XChaCha20Poly1305, dir: Direction, session: SessionId, counter: u64 }

impl Sealer {
    pub fn new(key: &[u8; 32], dir: Direction, session: SessionId) -> Self {
        Sealer { aead: XChaCha20Poly1305::new(key.into()), dir, session, counter: 0 }
    }
    pub fn session(&self) -> SessionId { self.session }

    pub fn seal(&mut self, challenge: &Challenge, plaintext: &[u8]) -> Vec<u8> {
        self.counter += 1;
        let mut frame = Vec::with_capacity(HEADER_LEN + plaintext.len() + 16);
        frame.extend_from_slice(MAGIC);
        frame.push(VERSION);
        frame.push(self.dir as u8);
        frame.extend_from_slice(&self.session);
        frame.extend_from_slice(&self.counter.to_be_bytes());
        frame.extend_from_slice(challenge);
        let ct = self.aead
            .encrypt(&nonce(&self.session, self.counter), Payload { msg: plaintext, aad: &frame })
            .expect("xchacha20poly1305 encrypt");
        frame.extend_from_slice(&ct);
        frame
    }
}

pub struct Opened { pub plaintext: Vec<u8>, pub new_session: bool, pub session: SessionId, pub counter: u64, pub challenge: Challenge }

/// The receiving side for one direction.
pub struct Opener {
    aead: XChaCha20Poly1305,
    dir: Direction,
    /// Required challenge (Windows receiver); None = accept and report (Mac side).
    challenge: Option<Challenge>,
    current: Option<(SessionId, u64)>,
    retired: VecDeque<SessionId>,
    /// Mac side only: the last counter of each recent session, most recent last.
    /// Sessions are never retired there, so one replayed frame from an old
    /// session cannot lock out the live one; trust is decided by the Ack echo.
    known: VecDeque<(SessionId, u64)>,
}

impl Opener {
    pub fn new(key: &[u8; 32], dir: Direction, challenge: Option<Challenge>) -> Self {
        Opener { aead: XChaCha20Poly1305::new(key.into()), dir, challenge, current: None, retired: VecDeque::new(), known: VecDeque::new() }
    }

    pub fn open(&mut self, frame: &[u8]) -> Result<Opened, FrameError> {
        if frame.len() < HEADER_LEN + 16 { return Err(FrameError::Short); }
        let (header, ct) = frame.split_at(HEADER_LEN);
        if &header[0..3] != MAGIC { return Err(FrameError::BadMagic); }
        if header[3] != VERSION { return Err(FrameError::BadVersion); }
        if header[4] != self.dir as u8 { return Err(FrameError::WrongDirection); }
        let session: SessionId = header[5..21].try_into().unwrap();
        let counter = u64::from_be_bytes(header[21..29].try_into().unwrap());
        let challenge: Challenge = header[29..45].try_into().unwrap();
        if self.challenge.is_some_and(|c| c != challenge) { return Err(FrameError::WrongChallenge); }
        // Authenticate before touching replay state, so forged frames change nothing.
        let plaintext = self.aead
            .decrypt(&nonce(&session, counter), Payload { msg: ct, aad: header })
            .map_err(|_| FrameError::Auth)?;
        if self.challenge.is_none() {
            let new_session = match self.known.iter().position(|(s, _)| *s == session) {
                Some(i) => {
                    let (_, last) = self.known.remove(i).unwrap();
                    if counter <= last { self.known.push_back((session, last)); return Err(FrameError::Replay); }
                    false
                }
                None => true,
            };
            self.known.push_back((session, counter));
            if self.known.len() > KNOWN_SESSIONS { self.known.pop_front(); }
            return Ok(Opened { plaintext, new_session, session, counter, challenge });
        }
        let new_session = match self.current {
            Some((s, last)) if s == session => {
                if counter <= last { return Err(FrameError::Replay); }
                false
            }
            _ => {
                if self.retired.contains(&session) { return Err(FrameError::RetiredSession); }
                if let Some((old, _)) = self.current {
                    self.retired.push_back(old);
                    if self.retired.len() > RETIRED_SESSIONS { self.retired.pop_front(); }
                }
                true
            }
        };
        self.current = Some((session, counter));
        Ok(Opened { plaintext, new_session, session, counter, challenge })
    }
}

fn nonce(session: &SessionId, counter: u64) -> XNonce {
    let mut n = [0u8; 24];
    n[..16].copy_from_slice(session);
    n[16..].copy_from_slice(&counter.to_be_bytes());
    n.into()
}

// ---- payloads ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent { pub seq: u32, pub key: u16, pub down: bool, pub repeat: bool }

/// Mac -> Windows. Carries the newest events (redundant history so a lost
/// datagram is recovered by the next one) and the full held-key snapshot as of
/// `last_seq`, so state converges even when history is not enough.
/// A heartbeat is the same payload with no events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeysPayload { pub last_seq: u32, pub events: Vec<KeyEvent>, pub held: Vec<u16> }

/// Windows -> Mac: liveness, the challenge (in the header), what was applied, and
/// the receiver's mouse activity: a running count of polls that saw the cursor
/// move, and how long ago the last one was (u32::MAX = never); and running
/// counts of its cursor's events at the outer left edge (`flow::WinEdgeEvent`);
/// and whether the MX4 is in its device list now, with running counts of its
/// arrivals and departures. No coordinates, no device names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckPayload {
    pub last_applied: u32, pub mouse_moves: u32, pub mouse_idle_ms: u32, pub entered: u32, pub departed: u32, pub reached: u32,
    pub mx4_present: bool, pub mx4_arrivals: u32, pub mx4_departures: u32,
    /// The Mac session id of the last frame the receiver accepted (zeros = none yet).
    pub echo: SessionId,
}

pub const MAX_EVENTS: usize = 32;
pub const MAX_HELD: usize = 32;

impl KeysPayload {
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.events.len() <= MAX_EVENTS && self.held.len() <= MAX_HELD);
        let mut b = Vec::with_capacity(6 + self.events.len() * 7 + self.held.len() * 2);
        b.extend_from_slice(&self.last_seq.to_be_bytes());
        b.push(self.events.len() as u8);
        for e in &self.events {
            b.extend_from_slice(&e.seq.to_be_bytes());
            b.extend_from_slice(&e.key.to_be_bytes());
            b.push(u8::from(e.down) | (u8::from(e.repeat) << 1));
        }
        b.push(self.held.len() as u8);
        for k in &self.held { b.extend_from_slice(&k.to_be_bytes()); }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self, FrameError> {
        let mut r = Reader(b);
        let last_seq = r.u32()?;
        let n = r.u8()? as usize;
        if n > MAX_EVENTS { return Err(FrameError::Payload); }
        let mut events = Vec::with_capacity(n);
        for _ in 0..n {
            let (seq, key, flags) = (r.u32()?, r.u16()?, r.u8()?);
            if flags & !0b11 != 0 || seq > last_seq { return Err(FrameError::Payload); }
            events.push(KeyEvent { seq, key, down: flags & 1 != 0, repeat: flags & 2 != 0 });
        }
        let h = r.u8()? as usize;
        if h > MAX_HELD { return Err(FrameError::Payload); }
        let mut held = Vec::with_capacity(h);
        for _ in 0..h { held.push(r.u16()?); }
        if !r.0.is_empty() { return Err(FrameError::Payload); }
        Ok(KeysPayload { last_seq, events, held })
    }
}

impl AckPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut b: Vec<u8> = [self.last_applied, self.mouse_moves, self.mouse_idle_ms, self.entered, self.departed, self.reached].iter().flat_map(|v| v.to_be_bytes()).collect();
        b.push(u8::from(self.mx4_present));
        b.extend_from_slice(&self.mx4_arrivals.to_be_bytes());
        b.extend_from_slice(&self.mx4_departures.to_be_bytes());
        b.extend_from_slice(&self.echo);
        b
    }
    pub fn decode(b: &[u8]) -> Result<Self, FrameError> {
        let mut r = Reader(b);
        let (last_applied, mouse_moves, mouse_idle_ms) = (r.u32()?, r.u32()?, r.u32()?);
        let (entered, departed, reached) = (r.u32()?, r.u32()?, r.u32()?);
        let mx4_present = match r.u8()? { 0 => false, 1 => true, _ => return Err(FrameError::Payload) };
        let (mx4_arrivals, mx4_departures) = (r.u32()?, r.u32()?);
        let echo: SessionId = r.take(16)?.try_into().unwrap();
        if !r.0.is_empty() { return Err(FrameError::Payload); }
        Ok(AckPayload { last_applied, mouse_moves, mouse_idle_ms, entered, departed, reached, mx4_present, mx4_arrivals, mx4_departures, echo })
    }
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], FrameError> {
        if self.0.len() < n { return Err(FrameError::Payload); }
        let (a, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, FrameError> { Ok(self.take(1)?[0]) }
    fn u16(&mut self) -> Result<u16, FrameError> { Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap())) }
    fn u32(&mut self) -> Result<u32, FrameError> { Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];
    const CH: Challenge = [9; 16];

    fn pair() -> (Sealer, Opener) {
        (Sealer::new(&KEY, Direction::MacToWin, [1; 16]), Opener::new(&KEY, Direction::MacToWin, Some(CH)))
    }

    #[test]
    fn roundtrip_and_ciphertext_hides_payload() {
        let (mut s, mut o) = pair();
        let msg = b"keys:secret-typing";
        let f = s.seal(&CH, msg);
        assert!(!f.windows(msg.len()).any(|w| w == msg));
        let opened = o.open(&f).unwrap();
        assert_eq!(opened.plaintext, msg);
        assert!(opened.new_session);
        assert!(!o.open(&s.seal(&CH, b"2")).unwrap().new_session);
    }

    #[test]
    fn tampering_and_wrong_key_fail_auth() {
        let (mut s, mut o) = pair();
        let f = s.seal(&CH, b"hello");
        for i in [0usize, 4, 10, 25, HEADER_LEN, f.len() - 1] {
            let mut t = f.clone();
            t[i] ^= 1;
            assert!(o.open(&t).is_err(), "byte {i} flip accepted");
        }
        let mut other = Opener::new(&[8; 32], Direction::MacToWin, Some(CH));
        assert_eq!(other.open(&f).err(), Some(FrameError::Auth));
        assert!(o.open(&f).is_ok()); // failed attempts left no state behind
    }

    #[test]
    fn replay_and_reorder_rejected() {
        let (mut s, mut o) = pair();
        let f1 = s.seal(&CH, b"1");
        let f2 = s.seal(&CH, b"2");
        o.open(&f2).unwrap();
        assert_eq!(o.open(&f1).err(), Some(FrameError::Replay));
        assert_eq!(o.open(&f2).err(), Some(FrameError::Replay));
    }

    #[test]
    fn retired_session_and_old_challenge_rejected() {
        let (mut s1, mut o) = pair();
        let old = s1.seal(&CH, b"old");
        o.open(&s1.seal(&CH, b"first")).unwrap();
        let mut s2 = Sealer::new(&KEY, Direction::MacToWin, [2; 16]); // Mac restarted
        assert!(o.open(&s2.seal(&CH, b"new")).unwrap().new_session);
        assert_eq!(o.open(&old).err(), Some(FrameError::RetiredSession));

        // Receiver restarted with a new challenge: everything captured before is dead.
        let mut o2 = Opener::new(&KEY, Direction::MacToWin, Some([10; 16]));
        assert_eq!(o2.open(&old).err(), Some(FrameError::WrongChallenge));
    }

    #[test]
    fn reflection_rejected() {
        let mut ack = Sealer::new(&KEY, Direction::WinToMac, [3; 16]);
        let (_, mut o) = pair();
        assert_eq!(o.open(&ack.seal(&CH, b"x")).err(), Some(FrameError::WrongDirection));
    }

    #[test]
    fn payload_codec() {
        let p = KeysPayload {
            last_seq: 9,
            events: vec![KeyEvent { seq: 8, key: 0x1E, down: true, repeat: false }, KeyEvent { seq: 9, key: 0x1E, down: true, repeat: true }],
            held: vec![0x1E, 0x15B],
        };
        assert_eq!(KeysPayload::decode(&p.encode()).unwrap(), p);
        let mut bad = p.encode();
        bad.push(0);
        assert_eq!(KeysPayload::decode(&bad).err(), Some(FrameError::Payload));
        assert_eq!(KeysPayload::decode(&p.encode()[..5]).err(), Some(FrameError::Payload));
        let ack = AckPayload { last_applied: 5, mouse_moves: 7, mouse_idle_ms: u32::MAX, entered: 1, departed: 2, reached: 3, mx4_present: true, mx4_arrivals: 4, mx4_departures: 3, echo: [5; 16] };
        assert_eq!(AckPayload::decode(&ack.encode()).unwrap(), ack);
        assert_eq!(AckPayload::decode(&ack.encode()[..24]), Err(FrameError::Payload), "a v3 Ack body is refused");
        let mut bad = ack.encode();
        bad[24] = 2;
        assert_eq!(AckPayload::decode(&bad), Err(FrameError::Payload), "present is 0 or 1");
        assert_eq!(AckPayload::decode(&5u32.to_be_bytes()), Err(FrameError::Payload), "a v1 Ack body is refused");
        assert_eq!(AckPayload::decode(&ack.encode()[..12]), Err(FrameError::Payload), "a v2 Ack body is refused");
    }

    #[test]
    fn random_sessions_differ() { assert_ne!(random16(), random16()); }
}
