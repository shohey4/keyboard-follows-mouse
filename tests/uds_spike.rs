//! Mode A spike: the bridge side of `send_user_command` over a real UNIX
//! datagram socket in a private temp dir. Karabiner is NOT involved; this only
//! measures our receive + parse + route cost and the socket's ordering/loss
//! under typing-like and burst loads. Run with `--nocapture` for the numbers.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};

use keyboard_follows_mouse::karabiner::{parse, Bridge, BridgeIn, Out, Typematic};
use keyboard_follows_mouse::keymap::WindowsProfile;
use keyboard_follows_mouse::router::{Hotkey, Router};

struct Dir(std::path::PathBuf);
impl Drop for Dir { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

fn private_dir(tag: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("kfm-spike-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
    Dir(d)
}

const KEYS: [&str; 6] = ["a", "s", "d", "f", "j", "k"];

fn payload(i: usize) -> Vec<u8> {
    let k = KEYS[(i / 2) % KEYS.len()];
    format!(r#"{{"kfm":1,"k":"{k}","d":{}}}"#, u8::from(i.is_multiple_of(2))).into_bytes()
}

fn set_rcvbuf(sock: &UnixDatagram, bytes: i32) -> i32 {
    use std::os::fd::AsRawFd;
    let fd = sock.as_raw_fd();
    let len = std::mem::size_of::<i32>() as libc::socklen_t;
    let rc = unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, (&bytes as *const i32).cast(), len) };
    assert_eq!(rc, 0, "setsockopt SO_RCVBUF");
    let (mut got, mut l) = (0i32, len);
    unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, (&mut got as *mut i32).cast(), &mut l) };
    got
}

fn pct(v: &mut [Duration], p: f64) -> Duration {
    v.sort();
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

#[test]
fn typing_rate_in_order_no_loss_low_latency() {
    let dir = private_dir("typing");
    let path = dir.0.join("user_command_receiver.sock");
    let rx = UnixDatagram::bind(&path).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    const N: usize = 4000;
    let sent_at: Vec<Instant> = Vec::with_capacity(N);
    let sent_at = std::sync::Arc::new(std::sync::Mutex::new(sent_at));
    let s2 = sent_at.clone();
    let p2 = path.clone();
    let sender = std::thread::spawn(move || {
        let tx = UnixDatagram::unbound().unwrap();
        for i in 0..N {
            s2.lock().unwrap().push(Instant::now());
            tx.send_to(&payload(i), &p2).unwrap();
            // ~ 2 ms between events = 250 events/s, far above human typing.
            std::thread::sleep(Duration::from_micros(2000));
        }
    });

    let mut bridge = Bridge::new(Router::new(WindowsProfile::identity(), Hotkey::default()), Typematic::default());
    bridge.on_link(true);
    bridge.on_input(BridgeIn::ToggleRequest, 0);
    let mut buf = [0u8; 512];
    let (mut latency, mut process) = (Vec::with_capacity(N), Vec::with_capacity(N));
    for i in 0..N {
        let n = rx.recv(&mut buf).expect("datagram lost or late");
        let got = Instant::now();
        assert_eq!(&buf[..n], payload(i).as_slice(), "out of order at {i}");
        let ev = parse(&buf[..n]).unwrap();
        let out = bridge.on_input(ev, i as u64);
        assert!(matches!(out.as_slice(), [Out::Remote { .. }]), "event {i} not forwarded: {out:?}");
        process.push(got.elapsed());
        latency.push(got - sent_at.lock().unwrap()[i]);
    }
    sender.join().unwrap();
    let (l50, l95, lmax) = (pct(&mut latency, 0.5), pct(&mut latency, 0.95), *latency.iter().max().unwrap());
    let (p50, p95) = (pct(&mut process, 0.5), pct(&mut process, 0.95));
    println!("uds typing: n={N} socket p50={l50:?} p95={l95:?} max={lmax:?}; parse+route p50={p50:?} p95={p95:?}");
    assert!(l95 < Duration::from_millis(5), "socket p95 {l95:?}");
    assert!(p95 < Duration::from_millis(1), "parse+route p95 {p95:?}");
}

fn burst(rcvbuf: Option<i32>) -> (usize, usize, usize) {
    let dir = private_dir(&format!("burst{}", rcvbuf.unwrap_or(0)));
    let path = dir.0.join("r.sock");
    let rx = UnixDatagram::bind(&path).unwrap();
    if let Some(b) = rcvbuf { println!("SO_RCVBUF set to {}", set_rcvbuf(&rx, b)); }
    let tx = UnixDatagram::unbound().unwrap();
    tx.set_nonblocking(true).unwrap();
    const N: usize = 2_000;
    let mut ok = 0;
    for i in 0..N { if tx.send_to(&payload(i), &path).is_ok() { ok += 1; } }
    rx.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 512];
    let mut received = 0;
    while rx.recv(&mut buf).is_ok() { received += 1; }
    assert_eq!(received, ok, "accepted datagrams must all arrive");
    (N, ok, received)
}

#[test]
fn larger_receive_buffer_absorbs_a_stalled_receiver() {
    let (n, default_ok, _) = burst(None);
    let (_, big_ok, _) = burst(Some(256 * 1024));
    println!("uds stalled-receiver burst of {n}: default buffer accepted {default_ok}, 256KiB accepted {big_ok}");
    assert!(big_ok > default_ok * 10, "raising SO_RCVBUF should hold far more pending events");
}

#[test]
fn burst_shows_whether_the_socket_drops() {
    // Informational: a sender that never waits (as a remapper might) against a
    // receiver that is busy. Reports how macOS AF_UNIX datagrams behave when
    // the receive buffer is full; the adapter must treat any loss as a gap.
    let dir = private_dir("burst");
    let path = dir.0.join("r.sock");
    let rx = UnixDatagram::bind(&path).unwrap();
    let tx = UnixDatagram::unbound().unwrap();
    tx.set_nonblocking(true).unwrap();
    const N: usize = 20_000;
    let (mut ok, mut errs) = (0usize, std::collections::BTreeMap::<String, usize>::new());
    for i in 0..N {
        match tx.send_to(&payload(i), &path) {
            Ok(_) => ok += 1,
            Err(e) => *errs.entry(format!("{:?}", e.kind())).or_default() += 1,
        }
    }
    rx.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 512];
    let mut received = 0;
    while rx.recv(&mut buf).is_ok() { received += 1; }
    println!("uds burst: sent_ok={ok} send_errors={errs:?} received={received} of {N}");
    assert_eq!(received, ok, "accepted datagrams must all arrive");
}
