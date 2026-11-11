//! Stage 6 rehearsal on this Mac only: the real kfm-mac and kfm-receive binaries
//! over UDP on 127.0.0.1 with a pre-shared key file. kfm-receive stays in dry-run
//! (counts, never injects); nothing leaves the loopback interface.
#![cfg(unix)]

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 { UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() }

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("kfm-udp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir(&d).unwrap();
    d
}

fn keygen(path: &Path) {
    assert!(Command::new(env!("CARGO_BIN_EXE_kfm-mac")).args(["keygen", "--out", path.to_str().unwrap()]).status().unwrap().success());
}

fn spawn(bin: &str, args: &[&str]) -> Child {
    Command::new(bin).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

fn out(c: Child) -> String {
    let o = c.wait_with_output().unwrap();
    assert!(o.status.success(), "{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Runs kfm-receive (dry-run) with `win_key` and kfm-mac with `mac_key`, sends the
/// synthetic script, and returns (mac stdout, win stdout).
fn run(tag: &str, same_key: bool) -> (String, String) {
    let d = dir(tag);
    let (k1, k2) = (d.join("k1"), d.join("k2"));
    keygen(&k1);
    keygen(&k2);
    let (wp, mp) = (free_port(), free_port());
    let (win_addr, mac_addr) = (format!("127.0.0.1:{wp}"), format!("127.0.0.1:{mp}"));
    let win_key = if same_key { &k1 } else { &k2 };
    let win = spawn(env!("CARGO_BIN_EXE_kfm-receive"), &["--bind", &win_addr, "--peer", &mac_addr, "--key-file", win_key.to_str().unwrap(), "--seconds", "3"]);
    let sock_dir = d.join("s");
    let mac = spawn(env!("CARGO_BIN_EXE_kfm-mac"), &["run", "--socket-dir", sock_dir.to_str().unwrap(), "--peer", &win_addr, "--bind", &mac_addr, "--key-file", k1.to_str().unwrap(), "--enable-send"]);
    let sock = sock_dir.join("user_command_receiver.sock");
    let t = Instant::now();
    while !sock.exists() { assert!(t.elapsed() < Duration::from_secs(5)); std::thread::sleep(Duration::from_millis(20)); }
    std::thread::sleep(Duration::from_millis(1500)); // heartbeat + ack: link up (or not); 400 ms was too short on a loaded Mac (link up at 494 ms seen)
    assert!(Command::new(env!("CARGO_BIN_EXE_kfm-mac")).args(["send-test", "--socket", sock.to_str().unwrap()]).status().unwrap().success());
    std::thread::sleep(Duration::from_millis(300));
    unsafe { libc::kill(mac.id() as i32, libc::SIGTERM) };
    let (m, w) = (out(mac), out(win));
    std::fs::remove_dir_all(&d).unwrap();
    for s in [&m, &w] { for leak in ["\"a\"", "left_shift", "international3"] { assert!(!s.contains(leak), "key name logged: {leak}\n{s}"); } }
    (m, w)
}

#[test]
fn mac_to_win_dry_run_authenticated_over_udp() {
    let (m, w) = run("ok", true);
    for want in ["link: UDP to the pinned peer", "destination: Windows", "[dry-run] would run: karabiner_cli --set-variables '{\"kfm_route\":1}'", "destination: Mac"] {
        assert!(m.contains(want), "mac missing {want:?}:\n{m}");
    }
    assert!(w.contains("dry-run (nothing injected)"), "{w}");
    assert!(w.contains("would have injected 2 down / 2 up"), "{w}");
    assert!(w.contains("rejected 0, foreign 0"), "{w}");
}

#[test]
fn wrong_key_never_links_and_windows_is_refused() {
    let (m, w) = run("badkey", false);
    assert!(m.contains("switch to Windows refused: link not confirmed"), "{m}");
    assert!(!m.contains("destination: Windows"), "{m}");
    assert!(w.contains("would have injected 0 down / 0 up"), "{w}");
    // The Mac cannot open the receiver's hello (wrong key), so it never gets a
    // challenge to send under: nothing authenticated reaches the receiver.
    assert!(w.contains("final: frames ok 0,"), "{w}");
}

/// An interactive zsh starts `cmd &` with SIGINT blocked (seen on 2026-10-31:
/// `kill -INT` left kfm-mac running). kfm-mac must unblock it and stop cleanly.
#[test]
fn sigint_stops_kfm_mac_even_when_inherited_blocked() {
    use std::os::unix::process::CommandExt;
    let d = dir("sigint");
    let sockdir = d.join("s");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kfm-mac"));
    cmd.args(["run", "--loopback", "--socket-dir", sockdir.to_str().unwrap()]).stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let sock = sockdir.join("user_command_receiver.sock");
    let t = Instant::now();
    while !sock.exists() { assert!(t.elapsed() < Duration::from_secs(5)); std::thread::sleep(Duration::from_millis(20)); }
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let t = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if t.elapsed() > Duration::from_secs(3) { let _ = child.kill(); panic!("kfm-mac ignored SIGINT"); }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("stopped:"), "{stdout}");
    assert!(!sockdir.exists(), "socket dir cleaned up");
    let _ = std::fs::remove_dir_all(&d);
}

/// Stage 7a arguments: no default edge (refused before any socket exists), and an
/// observing run prints the display list and a summary without coordinates.
#[test]
fn flow_observe_needs_an_edge_and_reports_counts_only() {
    let d = dir("flow");
    let sockdir = d.join("s");
    let out = Command::new(env!("CARGO_BIN_EXE_kfm-mac")).args(["run", "--loopback", "--socket-dir", sockdir.to_str().unwrap(), "--flow-observe"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--flow-edge"));
    assert!(!sockdir.exists(), "refused before creating the socket dir");
    let child = Command::new(env!("CARGO_BIN_EXE_kfm-mac"))
        .args(["run", "--loopback", "--socket-dir", sockdir.to_str().unwrap(), "--flow-observe", "--flow-edge", "right", "--flow-display", "all"])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let t = Instant::now();
    while !sockdir.join("user_command_receiver.sock").exists() { assert!(t.elapsed() < Duration::from_secs(5)); std::thread::sleep(Duration::from_millis(20)); }
    std::thread::sleep(Duration::from_millis(300));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    for want in ["flow 7a (observe only", "display 1:", "flow 7a summary: edges [", "flow 7a summary rule A:", "flow 7a summary rule C:"] { assert!(stdout.contains(want), "missing {want:?} in:\n{stdout}"); }
    assert!(!stdout.contains("route set"), "observing never changes the route");
    let _ = std::fs::remove_dir_all(&d);
}

/// Stage 7b: --flow-follow refuses to start without --apply-route, the Karabiner
/// endpoint, a peer, and the running launchd fail-safe -- before any socket exists.
#[test]
fn flow_follow_refuses_without_its_preconditions() {
    let d = dir("follow");
    let sockdir = d.join("s");
    let base = ["run", "--socket-dir", sockdir.to_str().unwrap(), "--flow-follow", "--flow-edge", "right", "--flow-display", "all"];
    let out = Command::new(env!("CARGO_BIN_EXE_kfm-mac")).args(base).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--flow-follow needs --apply-route"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!sockdir.exists(), "refused before creating the socket dir");
    let _ = std::fs::remove_dir_all(&d);
}

/// K6: if the receiver socket is removed or replaced (Karabiner restarted), kfm-mac
/// returns to the Mac, stops, prints the run summary, and leaves the stand-in alone.
#[test]
fn k6_socket_replaced_returns_to_the_mac_and_stops() {
    let d = dir("k6");
    let sockdir = d.join("s");
    let mut child = spawn(env!("CARGO_BIN_EXE_kfm-mac"), &["run", "--loopback", "--socket-dir", sockdir.to_str().unwrap()]);
    let sock = sockdir.join("user_command_receiver.sock");
    let t = Instant::now();
    while !sock.exists() { assert!(t.elapsed() < Duration::from_secs(5)); std::thread::sleep(Duration::from_millis(20)); }
    std::thread::sleep(Duration::from_millis(1000)); // loopback link up, with room for a loaded machine
    let tx = std::os::unix::net::UnixDatagram::unbound().unwrap();
    tx.send_to(br#"{"kfm":1,"e":"toggle"}"#, &sock).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    std::fs::remove_file(&sock).unwrap();
    std::fs::write(&sock, b"stand-in").unwrap();
    let t = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if t.elapsed() > Duration::from_secs(4) { let _ = child.kill(); panic!("kfm-mac did not stop after the socket was replaced"); }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Exits with an error on purpose, so a resident launchd job restarts it.
    let o = child.wait_with_output().unwrap();
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("(K6)"), "{o:?}");
    let o = String::from_utf8_lossy(&o.stdout).into_owned();
    let k6 = o.find("[k6 ").unwrap_or_else(|| panic!("{o}"));
    assert!(o[..k6].contains(r#"{"kfm_route":1}"#), "was on Windows first: {o}");
    assert!(o[k6..].contains(r#"{"kfm_route":0}"#), "back to the Mac: {o}");
    assert!(o.contains("run summary:") && o.contains("Windows (Hotkey)=1") && o.contains("(K6)"), "{o}");
    assert_eq!(std::fs::read(&sock).unwrap(), b"stand-in", "someone else's file at the path is not removed");
    let _ = std::fs::remove_dir_all(&d);
}

/// kfm-receive `--stop-file`: refuses to start if the file exists; stops (releasing
/// keys, printing the final line) once it appears.
#[test]
fn kfm_receive_stops_when_the_stop_file_appears() {
    let d = dir("stopfile");
    let k = d.join("k");
    keygen(&k);
    let stop = d.join("stop");
    let (wp, mp) = (free_port(), free_port());
    let args = ["--bind", &format!("127.0.0.1:{wp}"), "--peer", &format!("127.0.0.1:{mp}"), "--key-file", k.to_str().unwrap(), "--stop-file", stop.to_str().unwrap()].map(String::from);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    std::fs::write(&stop, b"").unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_kfm-receive")).args(&a).output().unwrap();
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("already exists"), "{o:?}");
    std::fs::remove_file(&stop).unwrap();
    let mut child = spawn(env!("CARGO_BIN_EXE_kfm-receive"), &a);
    std::thread::sleep(Duration::from_millis(500));
    assert!(child.try_wait().unwrap().is_none(), "still running without the file");
    std::fs::write(&stop, b"").unwrap();
    let t = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if t.elapsed() > Duration::from_secs(3) { let _ = child.kill(); panic!("kfm-receive ignored the stop file"); }
        std::thread::sleep(Duration::from_millis(20));
    }
    let o = out(child);
    assert!(o.contains("stop file found") && o.contains("final:"), "{o}");
    let _ = std::fs::remove_dir_all(&d);
}

fn wait_exit(child: &mut Child, secs: u64) {
    let t = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if t.elapsed() > Duration::from_secs(secs) { let _ = child.kill(); panic!("did not exit"); }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for(path: &Path) {
    let t = Instant::now();
    while !path.exists() { assert!(t.elapsed() < Duration::from_secs(5)); std::thread::sleep(Duration::from_millis(20)); }
}

/// Resident: the pause file keeps keys on the Mac (and refuses Fn+F12) until removed.
#[test]
fn pause_file_returns_to_the_mac_and_blocks_windows() {
    let d = dir("pause");
    let (sockdir, pause) = (d.join("s"), d.join("paused"));
    let mut child = spawn(env!("CARGO_BIN_EXE_kfm-mac"), &["run", "--loopback", "--socket-dir", sockdir.to_str().unwrap(), "--pause-file", pause.to_str().unwrap()]);
    let sock = sockdir.join("user_command_receiver.sock");
    wait_for(&sock);
    std::thread::sleep(Duration::from_millis(1000));
    let tx = std::os::unix::net::UnixDatagram::unbound().unwrap();
    tx.send_to(br#"{"kfm":1,"e":"toggle"}"#, &sock).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(&pause, b"").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    tx.send_to(br#"{"kfm":1,"e":"toggle"}"#, &sock).unwrap(); // refused while paused
    std::thread::sleep(Duration::from_millis(300));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    wait_exit(&mut child, 3);
    let o = out(child);
    let p = o.find("[pause ").unwrap_or_else(|| panic!("{o}"));
    assert!(o[..p].contains(r#"{"kfm_route":1}"#), "{o}");
    assert!(o[p..].contains(r#"{"kfm_route":0}"#), "back to the Mac: {o}");
    assert!(!o[p..].contains(r#"{"kfm_route":1}"#), "no Windows while paused: {o}");
    let _ = std::fs::remove_dir_all(&d);
}

/// Resident: a Hold longer than --hold-fallback-ms returns the keyboard to the Mac.
#[test]
fn long_hold_falls_back_to_the_mac() {
    let d = dir("holdfb");
    let k = d.join("k");
    keygen(&k);
    let (wp, mp) = (free_port(), free_port());
    let (win_addr, mac_addr) = (format!("127.0.0.1:{wp}"), format!("127.0.0.1:{mp}"));
    let mut win = spawn(env!("CARGO_BIN_EXE_kfm-receive"), &["--bind", &win_addr, "--peer", &mac_addr, "--key-file", k.to_str().unwrap()]);
    let sockdir = d.join("s");
    let mut mac = spawn(env!("CARGO_BIN_EXE_kfm-mac"), &["run", "--socket-dir", sockdir.to_str().unwrap(), "--peer", &win_addr, "--bind", &mac_addr, "--key-file", k.to_str().unwrap(), "--enable-send", "--hold-fallback-ms", "600"]);
    let sock = sockdir.join("user_command_receiver.sock");
    wait_for(&sock);
    std::thread::sleep(Duration::from_millis(1500));
    let tx = std::os::unix::net::UnixDatagram::unbound().unwrap();
    tx.send_to(br#"{"kfm":1,"e":"toggle"}"#, &sock).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let _ = win.kill(); // the receiver vanishes: link down -> Held
    let _ = win.wait();
    std::thread::sleep(Duration::from_millis(1500));
    unsafe { libc::kill(mac.id() as i32, libc::SIGTERM) };
    wait_exit(&mut mac, 3);
    let o = out(mac);
    let h = o.find("[hold ").unwrap_or_else(|| panic!("{o}"));
    assert!(o[..h].contains("destination: Held (LinkDown)"), "{o}");
    assert!(o[h..].contains("destination: Mac (LinkDown)") && o[h..].contains(r#"{"kfm_route":0}"#), "{o}");
    let _ = std::fs::remove_dir_all(&d);
}

/// Resident restart: a stale socket (nobody receiving) is removed; a live one is left alone.
#[test]
fn resident_clears_only_a_stale_socket() {
    let d = dir("stale");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap(); // like Karabiner's socket dir
    let sock = d.join("user_command_receiver.sock");
    let pause = d.join("paused");
    let args = ["run", "--socket-path", sock.to_str().unwrap(), "--use-karabiner-endpoint", "--resident", "--win-modmap", "off", "--pause-file", pause.to_str().unwrap()];
    let live = std::os::unix::net::UnixDatagram::bind(&sock).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_kfm-mac")).args(args).output().unwrap();
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("in use"), "{o:?}");
    assert!(sock.exists());
    drop(live); // the file stays, nobody receives: stale
    let mut child = spawn(env!("CARGO_BIN_EXE_kfm-mac"), &args);
    std::thread::sleep(Duration::from_millis(800));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    wait_exit(&mut child, 3);
    let o = out(child);
    assert!(o.contains("removed a stale receiver socket"), "{o}");
    assert!(!sock.exists(), "our own socket is cleaned up on exit");
    let _ = std::fs::remove_dir_all(&d);
}

/// Rule E over UDP: kfm-receive reports the MX4 (a stand-in file off Windows) and
/// kfm-mac logs each arrival / departure once; one already there at start is
/// not an arrival. Not following, so it only says what it would do.
#[test]
fn mx4_arrivals_and_departures_reach_the_mac() {
    let d = dir("mx4");
    let (k, mx4) = (d.join("k"), d.join("mx4"));
    keygen(&k);
    std::fs::write(&mx4, b"").unwrap(); // connected to Windows at start
    let (wp, mp) = (free_port(), free_port());
    let (win_addr, mac_addr) = (format!("127.0.0.1:{wp}"), format!("127.0.0.1:{mp}"));
    let win = Command::new(env!("CARGO_BIN_EXE_kfm-receive")).args(["--bind", &win_addr, "--peer", &mac_addr, "--key-file", k.to_str().unwrap(), "--seconds", "5", "--mx4-id", "test-mouse"])
        .env("KFM_TEST_MX4_FILE", &mx4).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let sockdir = d.join("s");
    let mut mac = spawn(env!("CARGO_BIN_EXE_kfm-mac"), &["run", "--socket-dir", sockdir.to_str().unwrap(), "--peer", &win_addr, "--bind", &mac_addr, "--key-file", k.to_str().unwrap(), "--enable-send"]);
    wait_for(&sockdir.join("user_command_receiver.sock"));
    std::thread::sleep(Duration::from_millis(1500));
    std::fs::remove_file(&mx4).unwrap(); // Easy-Switch to the Mac
    std::thread::sleep(Duration::from_millis(800));
    std::fs::write(&mx4, b"").unwrap(); // and back to Windows
    std::thread::sleep(Duration::from_millis(800));
    unsafe { libc::kill(mac.id() as i32, libc::SIGTERM) };
    wait_exit(&mut mac, 3);
    let (m, w) = (out(mac), out(win));
    let _ = std::fs::remove_dir_all(&d);
    let lines: Vec<&str> = m.lines().filter(|l| l.starts_with("[mx4 ")).collect();
    assert_eq!(lines.len(), 2, "{m}");
    assert!(lines[0].ends_with("arrived 0, left 1, now absent: WOULD SWITCH to Mac"), "{m}");
    assert!(lines[1].ends_with("arrived 1, left 0, now present: WOULD SWITCH to Windows"), "{m}");
    assert!(m.contains("MX4 on Windows: arrived 1, left 1"), "{m}");
    assert!(!m.contains("destination: Windows"), "not following: nothing moves\n{m}");
    assert!(w.contains("MX4 watch: raw-input mouse whose device path contains \"test-mouse\", every 200 ms (now present)"), "{w}");
    assert!(w.contains("mx4 arrived 1, left 1"), "{w}");
}

/// Invalid layout or key configuration fails closed before anything listens.
#[test]
fn invalid_layout_and_keymap_fail_closed() {
    let d = std::env::temp_dir().join(format!("kfm-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let k = d.join("k");
    keygen(&k);
    let port = free_port();
    let base = ["--bind", "127.0.0.1:0", "--peer", &format!("127.0.0.1:{port}"), "--key-file", k.to_str().unwrap(), "--seconds", "1"];
    for (extra, want) in [(vec!["--flow-edge", "diagonal"], "--flow-edge"), (vec!["--flow-display", "99"], "--flow-display")] {
        let o = Command::new(env!("CARGO_BIN_EXE_kfm-receive")).args(base).args(&extra).output().unwrap();
        assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains(want), "{extra:?}: {}", String::from_utf8_lossy(&o.stderr));
    }
    let o = Command::new(env!("CARGO_BIN_EXE_kfm-receive")).args(base).args(["--flow-edge", "top"]).output().unwrap();
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("watching the Top edge"), "{}", String::from_utf8_lossy(&o.stdout));
    let bad = d.join("bad.json");
    std::fs::write(&bad, r#"{"hotkey":"f99"}"#).unwrap();
    for bin in [env!("CARGO_BIN_EXE_kfm-karabiner"), env!("CARGO_BIN_EXE_kfm-mac")] {
        let o = Command::new(bin).arg("rule").env("KFM_KEYMAP", &bad).output().unwrap();
        assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("hotkey"), "{bin}: {}", String::from_utf8_lossy(&o.stderr));
    }
    let good = d.join("good.json");
    std::fs::write(&good, r#"{"hotkey":"f15"}"#).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_kfm-karabiner")).arg("rule").env("KFM_KEYMAP", &good).output().unwrap();
    let rule = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && rule.contains("\"f15\"") && !rule.contains("\"f13\""), "{rule}");
    // A rule planned with the default hotkey is reported when the keymap now says f15.
    let kj = d.join("karabiner.json");
    std::fs::write(&kj, r#"{"profiles":[{"name":"p","selected":true,"complex_modifications":{"rules":[]}}]}"#).unwrap();
    let planned = d.join("planned.json");
    let o = Command::new(env!("CARGO_BIN_EXE_kfm-karabiner")).args(["plan", "--config", kj.to_str().unwrap(), "--out", planned.to_str().unwrap()]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let ok = Command::new(env!("CARGO_BIN_EXE_kfm-karabiner")).args(["preflight", "--config", planned.to_str().unwrap()]).output().unwrap();
    assert!(ok.status.success() && String::from_utf8_lossy(&ok.stdout).contains("hotkey: configured f13, installed rule f13"), "{}", String::from_utf8_lossy(&ok.stdout));
    let bad = Command::new(env!("CARGO_BIN_EXE_kfm-karabiner")).args(["preflight", "--config", planned.to_str().unwrap()]).env("KFM_KEYMAP", &good).output().unwrap();
    assert!(!bad.status.success() && String::from_utf8_lossy(&bad.stdout).contains("not the configured f15"), "{}", String::from_utf8_lossy(&bad.stdout));
    let _ = std::fs::remove_dir_all(&d);
}
