//! Synthetic end-to-end on this Mac only: Karabiner-shaped datagrams -> bridge
//! -> MacLink -> real UDP on 127.0.0.1 -> WinLink -> recording injector. No
//! Karabiner, no real keys, nothing leaves the loopback interface.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use keyboard_follows_mouse::karabiner::{parse, Bridge, BridgeIn, Out, Typematic};
use keyboard_follows_mouse::keymap::WindowsProfile;
use keyboard_follows_mouse::keys::{hid, win_key_for_usage, WinKey, VK_IME_ON};
use keyboard_follows_mouse::link::Injector;
use keyboard_follows_mouse::router::{Dest, Hotkey, Router};
use keyboard_follows_mouse::transport::{LinkChange, MacLink, Timing, WinLink};

#[derive(Default)]
struct Rec(Vec<(WinKey, bool)>);
impl Injector for Rec { fn inject(&mut self, k: WinKey, d: bool) { self.0.push((k, d)); } }

struct Harness {
    start: Instant,
    mac_sock: UdpSocket,
    win_sock: UdpSocket,
    bridge: Bridge,
    mac: MacLink,
    win: WinLink,
    rec: Rec,
    /// Fault injection.
    drop_acks: bool,
    corrupt_every: Option<usize>,
    sent: usize,
    pub dests: Vec<Dest>,
}

impl Harness {
    fn new() -> Self {
        let key = [9u8; 32];
        let mac_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let win_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        for s in [&mac_sock, &win_sock] { s.set_read_timeout(Some(Duration::from_millis(1))).unwrap(); }
        Harness {
            start: Instant::now(), mac_sock, win_sock,
            bridge: Bridge::new(Router::new(WindowsProfile::identity(), Hotkey::default()), Typematic::default()),
            mac: MacLink::new(key, Timing::default()), win: WinLink::new(key, Timing::default()),
            rec: Rec::default(), drop_acks: false, corrupt_every: None, sent: 0, dests: vec![],
        }
    }

    fn now(&self) -> u64 { self.start.elapsed().as_millis() as u64 }

    fn mac_send(&mut self, mut frame: Vec<u8>) {
        self.sent += 1;
        if self.corrupt_every.is_some_and(|n| self.sent.is_multiple_of(n)) { let i = frame.len() / 2; frame[i] ^= 0xA5; }
        self.mac_sock.send_to(&frame, self.win_sock.local_addr().unwrap()).unwrap();
    }

    fn handle(&mut self, outs: Vec<Out>) {
        let mut queue = outs;
        while let Some(o) = (!queue.is_empty()).then(|| queue.remove(0)) {
            match o {
                Out::Remote { key, down, repeat } => { let now = self.now(); if let Some(f) = self.mac.key(key, down, repeat, now) { self.mac_send(f); } }
                Out::Dest { dest, .. } => self.dests.push(dest),
                _ => {}
            }
        }
    }

    /// One scheduler round: both sides receive, tick and send.
    fn step(&mut self) {
        let now = self.now();
        let mut buf = [0u8; 512];
        // Windows side
        while let Ok((n, from)) = self.win_sock.recv_from(&mut buf) {
            assert_eq!(from, self.mac_sock.local_addr().unwrap());
            if let Ok(ack) = self.win.on_datagram(&buf[..n], now, &mut self.rec)
                && !self.drop_acks { self.win_sock.send_to(&ack, from).unwrap(); }
        }
        if let Some(hello) = self.win.tick(now, &mut self.rec)
            && !self.drop_acks { self.win_sock.send_to(&hello, self.mac_sock.local_addr().unwrap()).unwrap(); }
        // Mac side
        let mut changes = vec![];
        while let Ok((n, _)) = self.mac_sock.recv_from(&mut buf) { changes.extend(self.mac.on_datagram(&buf[..n], now)); }
        let (hb, c) = self.mac.tick(now);
        changes.extend(c);
        if let Some(f) = hb { self.mac_send(f); }
        let outs: Vec<Out> = changes.into_iter().flat_map(|c| self.bridge.on_link(c == LinkChange::Up)).collect();
        self.handle(outs);
        let outs = self.bridge.tick(now);
        self.handle(outs);
    }

    fn run_for(&mut self, ms: u64) {
        let until = self.now() + ms;
        while self.now() < until { self.step(); std::thread::sleep(Duration::from_millis(2)); }
    }

    fn karabiner(&mut self, datagram: &str) -> Result<(), keyboard_follows_mouse::karabiner::ParseError> {
        let ev = parse(datagram.as_bytes())?;
        let outs = self.bridge.on_input(ev, self.now());
        self.handle(outs);
        self.step();
        Ok(())
    }

    fn held_on_windows(&self) -> std::collections::BTreeSet<WinKey> {
        let mut s = std::collections::BTreeSet::new();
        for (k, d) in &self.rec.0 { if *d { s.insert(*k); } else { s.remove(k); } }
        s
    }
}

fn w(u: keyboard_follows_mouse::keys::Usage) -> WinKey { win_key_for_usage(u).unwrap() }
fn key(name: &str, down: bool) -> String { format!(r#"{{"kfm":1,"k":"{name}","d":{}}}"#, u8::from(down)) }
const TOGGLE: &str = r#"{"kfm":1,"e":"toggle"}"#;
const ESCAPE: &str = r#"{"kfm":1,"e":"escape"}"#;

fn up_and_on_windows() -> Harness {
    let mut h = Harness::new();
    h.karabiner(TOGGLE).unwrap();
    assert_eq!(h.bridge.dest(), Dest::Mac, "refused before the link is confirmed");
    h.run_for(120);
    assert!(h.mac.is_up());
    h.karabiner(TOGGLE).unwrap();
    assert_eq!(h.bridge.dest(), Dest::Windows);
    assert!(h.bridge.karabiner_remote());
    h
}

#[test]
fn typing_modifiers_and_ime_over_udp_loopback() {
    let mut h = up_and_on_windows();
    for m in [key("left_control", true), key("c", true), key("c", false), key("left_control", false), key("lang1", true), key("lang1", false)] {
        h.karabiner(&m).unwrap();
    }
    h.run_for(80);
    let ime = WinKey::Vk(VK_IME_ON);
    assert_eq!(h.rec.0, vec![(w(hid::LCTRL), true), (w(hid::C), true), (w(hid::C), false), (w(hid::LCTRL), false), (ime, true), (ime, false)]);
    // Unsupported JIS key: rejected at parse, never converted to something else.
    assert!(h.karabiner(&key("international3", true)).is_err());
    assert!(h.held_on_windows().is_empty());
}

#[test]
fn corrupted_datagrams_are_rejected_and_state_converges() {
    let mut h = up_and_on_windows();
    h.corrupt_every = Some(3);
    for i in 0..30 {
        let k = ["a", "s", "d"][i % 3];
        h.karabiner(&key(k, true)).unwrap();
        if i % 2 == 0 { h.karabiner(&key(k, false)).unwrap(); }
    }
    for k in ["a", "s", "d"] { h.karabiner(&key(k, false)).unwrap(); }
    h.corrupt_every = None;
    h.run_for(150);
    assert!(h.win.rejected > 0, "corruption was exercised");
    assert!(h.held_on_windows().is_empty(), "nothing stays down after corruption: {:?}", h.held_on_windows());
}

#[test]
fn half_open_link_holds_then_recovers() {
    let mut h = up_and_on_windows();
    h.karabiner(&key("left_shift", true)).unwrap();
    h.run_for(30);
    assert_eq!(h.held_on_windows().len(), 1);
    h.drop_acks = true; // Windows -> Mac lost
    h.run_for(450);
    assert_eq!(h.bridge.dest(), Dest::Held);
    assert!(h.bridge.karabiner_remote(), "Karabiner keeps the Mac suppressed while held");
    assert!(h.held_on_windows().is_empty(), "Shift released on Windows (best-effort up or watchdog)");
    let before = h.rec.0.len();
    h.karabiner(&key("p", true)).unwrap();
    h.karabiner(&key("p", false)).unwrap();
    h.karabiner(&key("left_shift", false)).unwrap();
    h.run_for(30);
    assert_eq!(h.rec.0.len(), before, "held input reaches neither host");
    h.drop_acks = false;
    h.run_for(150);
    assert_eq!(h.bridge.dest(), Dest::Windows);
    h.karabiner(&key("a", true)).unwrap();
    h.karabiner(&key("a", false)).unwrap();
    h.run_for(30);
    assert_eq!(&h.rec.0[before..], &[(w(hid::A), true), (w(hid::A), false)]);
}

#[test]
fn escape_releases_and_returns_to_mac() {
    let mut h = up_and_on_windows();
    h.karabiner(&key("left_command", true)).unwrap();
    h.karabiner(&key("v", true)).unwrap();
    h.karabiner(ESCAPE).unwrap();
    h.run_for(60);
    assert_eq!(h.bridge.dest(), Dest::Mac);
    assert!(!h.bridge.karabiner_remote());
    assert!(h.held_on_windows().is_empty());
}

/// K1/K5 synthetic: Karabiner delivers a key-up whose key-down went to the Mac
/// (route switched while held). Surfaced as a Mac-bound drop, never sent to Windows.
#[test]
fn k1_up_without_down_is_surfaced_not_forwarded() {
    let mut h = up_and_on_windows();
    let before = h.rec.0.len();
    let outs = h.bridge.on_input(BridgeIn::Key { usage: hid::A, down: false }, h.now());
    assert_eq!(outs, vec![Out::MacKeyDropped]);
    h.run_for(30);
    assert_eq!(h.rec.0.len(), before);
}

/// K2 synthetic: a key-up datagram lost between Karabiner and the bridge. The
/// bridge cannot know; the key stays down on Windows until F13 (escape).
#[test]
fn k2_lost_karabiner_key_up_is_cleared_by_escape() {
    let mut h = up_and_on_windows();
    h.karabiner(&key("b", true)).unwrap();
    // ("b" up never arrives)
    h.run_for(600);
    assert_eq!(h.held_on_windows().len(), 1, "documented risk: stays down while the link is healthy");
    h.karabiner(ESCAPE).unwrap();
    h.run_for(60);
    assert!(h.held_on_windows().is_empty());
}

/// The dry-run binary end to end: private socket, loopback receiver, synthetic datagrams.
#[cfg(unix)] // spawns kfm-mac, which is macOS-only
#[test]
fn dry_run_binary_with_send_test() {
    let dir = std::env::temp_dir().join(format!("kfm-bin-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_kfm-mac"))
        .args(["run", "--loopback", "--socket-dir", dir.to_str().unwrap()])
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .spawn().unwrap();
    let sock = dir.join("user_command_receiver.sock");
    let t = Instant::now();
    while !sock.exists() { assert!(t.elapsed() < Duration::from_secs(5)); std::thread::sleep(Duration::from_millis(20)); }
    std::thread::sleep(Duration::from_millis(300)); // let the loopback link come up
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_kfm-mac")).args(["send-test", "--socket", sock.to_str().unwrap()]).status().unwrap();
    assert!(st.success());
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    for want in ["destination: Windows", "[dry-run] would run: karabiner_cli --set-variables '{\"kfm_route\":1}'", "destination: Mac",
                 "loopback receiver injected: 2 down / 2 up", "6 events, 1 rejected", "RECOVERY"] {
        assert!(stdout.contains(want), "missing {want:?} in:\n{stdout}");
    }
    for leak in ["\"a\"", "left_shift", "international3"] { assert!(!stdout.contains(leak), "key name logged: {leak}"); }
    assert!(!dir.exists(), "socket dir cleaned up");
}

/// Stage 7a over real UDP on 127.0.0.1: the receiver's mouse movement and its
/// left-edge entry travel in its authenticated Acks and drive rules A and C.
#[test]
fn flow_observer_switches_on_edge_then_receiver_mouse_over_udp() {
    use keyboard_follows_mouse::flow::{FlowConfig, Note, Observer, PointerEvent, Rule, WinEdgeEvent};
    use keyboard_follows_mouse::transport::PeerDelta;
    let (ms, ws) = (UdpSocket::bind("127.0.0.1:0").unwrap(), UdpSocket::bind("127.0.0.1:0").unwrap());
    let key = [9u8; 32];
    let (mut mac, mut win) = (MacLink::new(key, Timing::default()), WinLink::new(key, Timing::default()));
    let mut inj = Rec::default();
    let mut buf = [0u8; 512];
    let mut obs = Observer::new(FlowConfig::default(), 2);
    // The receiver's hello tells the Mac its challenge; after that, an Ack counts
    // only as the answer to the Mac's own frame (it echoes the Mac's session).
    let hello = win.tick(0, &mut inj).expect("a hello is due");
    ws.send_to(&hello, ms.local_addr().unwrap()).unwrap();
    let (n, _) = ms.recv_from(&mut buf).unwrap();
    mac.on_datagram(&buf[..n], 0);
    let mut exchange = |mac: &mut MacLink, win: &mut WinLink, now: u64| {
        let (hb, _) = mac.tick(now);
        ms.send_to(&hb.expect("a heartbeat is due"), ws.local_addr().unwrap()).unwrap();
        let (n, _) = ws.recv_from(&mut buf).unwrap();
        let ack = win.on_datagram(&buf[..n], now, &mut inj).expect("the receiver accepts the heartbeat");
        ws.send_to(&ack, ms.local_addr().unwrap()).unwrap();
        let (n, _) = ms.recv_from(&mut buf).unwrap();
        mac.on_datagram(&buf[..n], now);
    };
    exchange(&mut mac, &mut win, 0);
    assert_eq!(mac.take_peer(), PeerDelta::default(), "baseline");
    assert_eq!(obs.on_pointer(PointerEvent::EdgeArrived(1), 1000, Some(0.0)), vec![Note::Edge { display: 1 }]);
    win.edge_event(WinEdgeEvent::Entered);
    win.mouse_moved(1100);
    exchange(&mut mac, &mut win, 1150);
    assert_eq!(mac.take_peer(), PeerDelta { moved: true, entered: 1, departed: 0, reached: 0, ..Default::default() });
    assert!(obs.on_win_edge(WinEdgeEvent::Entered, 1150).contains(&Note::ToWindows { rule: Rule::C, display: Some(1), after_edge_ms: Some(150) }));
    let notes = obs.on_win_activity(1150);
    assert!(notes.contains(&Note::ToWindows { rule: Rule::A, display: Some(1), after_edge_ms: Some(150) }), "{notes:?}");
    assert!(obs.on_pointer(PointerEvent::Moved(Some(0)), 3000, Some(900.0)).contains(&Note::ToMac { rule: Rule::A, display: Some(0), after_windows_ms: Some(1850), from_edge_pt: Some(900) }));
}
