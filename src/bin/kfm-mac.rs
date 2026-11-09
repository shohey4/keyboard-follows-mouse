//! Mac bridge for mode A (Karabiner `send_user_command` -> router -> Windows).
//! Dry-run by default: a private socket in a fresh temp dir, no network, and
//! route changes are only printed. Key names/contents are never logged.
//!
//!   kfm-mac keygen --out PATH
//!   kfm-mac run [--loopback] [--socket-dir DIR]
//!               [--peer IP:PORT --bind IP:PORT --key-file PATH --enable-send]
//!               [--socket-path PATH --use-karabiner-endpoint] [--apply-route]
//!   kfm-mac send-test --socket PATH      (synthetic datagrams for a dry run)
//!   kfm-mac secure-probe                 (K3: one Secure Input poll, printed)
//!   run ... --flow-observe --flow-edge left|right|top|bottom [--flow-display all|N[,M]]
//!       stage 7a: records what the Flow follower would decide (times and
//!       counts only, never coordinates or keys); it never changes the route.
//!   run ... --flow-follow (same flow arguments)
//!       stage 7b: rule C moves the route for real. Needs --apply-route, the
//!       Karabiner endpoint, a --peer, and the launchd fail-safe running. While
//!       macOS Secure Input is on, nothing moves to Windows (K3), except when
//!       loginwindow holds it while the screen is unlocked (a stuck hold).
//!       Rule E also follows the MX4: it appearing in the Windows PC's device
//!       list moves to Windows, leaving it moves to the Mac (Easy-Switch, and
//!       Flow too); `--no-easy-switch` turns rule E off. Both only act on a
//!       change, so Fn+F12 holds until the next one.

#[cfg(unix)]
fn main() -> std::process::ExitCode { unix::main() }

#[cfg(not(unix))]
fn main() { eprintln!("kfm-mac runs on macOS only"); std::process::exit(2); }

#[cfg(unix)]
mod unix {
    use std::net::{SocketAddr, UdpSocket};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixDatagram;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use keyboard_follows_mouse::flow::{rule_e, Decisions, Edge, FlowConfig, Note, Observer, PointerTracker, Rect, Rule, WinEdgeEvent};
    use keyboard_follows_mouse::karabiner::{parse, secure_verdict, Bridge, Out, SecureVerdict, Typematic};
    use keyboard_follows_mouse::keymap::WindowsProfile;
    use keyboard_follows_mouse::keys::WinKey;
    use keyboard_follows_mouse::link::Injector;
    use keyboard_follows_mouse::router::{Hotkey, Router, Target};
    use keyboard_follows_mouse::transport::{LinkChange, MacLink, PeerDelta, Timing, WinLink};
    use keyboard_follows_mouse::keyfile;

    const KARABINER_CLI: &str = "/Library/Application Support/org.pqrs/Karabiner-Elements/bin/karabiner_cli";
    const RCVBUF: i32 = 256 * 1024;
    static STOP: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) { STOP.store(true, Ordering::SeqCst); }

    fn recovery() -> String {
        format!("RECOVERY: if the keyboard stops typing on the Mac, press F13 on it, or run:\n  '{KARABINER_CLI}' --set-variables '{{\"kfm_route\":0}}'")
    }

    fn arg(args: &[String], name: &str) -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    }
    fn flag(args: &[String], name: &str) -> bool { args.iter().any(|a| a == name) }

    fn set_rcvbuf(sock: &UnixDatagram) -> i32 {
        use std::os::fd::AsRawFd;
        let len = std::mem::size_of::<i32>() as libc::socklen_t;
        let fd = sock.as_raw_fd();
        unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, (&RCVBUF as *const i32).cast(), len) };
        let (mut got, mut l) = (0i32, len);
        unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, (&mut got as *mut i32).cast(), &mut l) };
        got
    }

    /// The socket directory must be ours and owner-only.
    fn check_dir(dir: &Path) -> Result<(), String> {
        let md = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if md.uid() != unsafe { libc::getuid() } { return Err(format!("{} is not owned by this user", dir.display())); }
        if md.permissions().mode() & 0o077 != 0 { return Err(format!("{} must be mode 0700", dir.display())); }
        Ok(())
    }

    /// Counts what a Windows receiver would inject (loopback dry run).
    #[derive(Default)]
    struct CountingInjector { downs: u64, ups: u64 }
    impl Injector for CountingInjector {
        fn inject(&mut self, _: WinKey, down: bool) { if down { self.downs += 1 } else { self.ups += 1 } }
    }

    #[allow(clippy::large_enum_variant)] // one instance per process
    enum Net {
        None,
        Loopback { link: MacLink, win: WinLink, inj: CountingInjector },
        Udp { link: MacLink, sock: UdpSocket, peer: SocketAddr },
    }

    impl Net {
        fn send_key(&mut self, key: WinKey, down: bool, repeat: bool, now: u64) -> Vec<LinkChange> {
            match self {
                Net::None => vec![],
                Net::Loopback { link, win, inj } => {
                    let mut ch = vec![];
                    if let Some(f) = link.key(key, down, repeat, now)
                        && let Ok(ack) = win.on_datagram(&f, now, inj) { ch.extend(link.on_datagram(&ack, now)); }
                    ch
                }
                Net::Udp { link, sock, peer } => {
                    if let Some(f) = link.key(key, down, repeat, now) { let _ = sock.send_to(&f, *peer); }
                    vec![]
                }
            }
        }

        fn is_up(&self) -> bool {
            match self { Net::Udp { link, .. } | Net::Loopback { link, .. } => link.is_up(), Net::None => false }
        }

        fn ages(&self, now: u64) -> (Option<u64>, Option<u64>) {
            match self { Net::Udp { link, .. } | Net::Loopback { link, .. } => link.ages(now), Net::None => (None, None) }
        }

        fn take_peer(&mut self) -> PeerDelta {
            match self { Net::Udp { link, .. } | Net::Loopback { link, .. } => link.take_peer(), Net::None => PeerDelta::default() }
        }

        fn poll(&mut self, now: u64) -> Vec<LinkChange> {
            let mut ch = vec![];
            match self {
                Net::None => {}
                Net::Loopback { link, win, inj } => {
                    if let Some(hello) = win.tick(now, inj) { ch.extend(link.on_datagram(&hello, now)); }
                    let (hb, c) = link.tick(now);
                    ch.extend(c);
                    if let Some(f) = hb && let Ok(ack) = win.on_datagram(&f, now, inj) { ch.extend(link.on_datagram(&ack, now)); }
                }
                Net::Udp { link, sock, peer } => {
                    let mut buf = [0u8; 512];
                    while let Ok((n, from)) = sock.recv_from(&mut buf) {
                        if from == *peer { ch.extend(link.on_datagram(&buf[..n], now)); }
                    }
                    let (hb, c) = link.tick(now);
                    ch.extend(c);
                    if let Some(f) = hb { let _ = sock.send_to(&f, *peer); }
                }
            }
            ch
        }
    }

    /// CoreGraphics reads for stage 7a. Reading the pointer location needs no
    /// permission (no event tap).
    mod cg {
        use keyboard_follows_mouse::flow::Rect;
        use std::ffi::c_void;
        #[repr(C)] #[derive(Clone, Copy)] struct Point { x: f64, y: f64 }
        #[repr(C)] #[derive(Clone, Copy)] struct Size { w: f64, h: f64 }
        #[repr(C)] #[derive(Clone, Copy)] struct CgRect { o: Point, s: Size }
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGEventCreate(source: *const c_void) -> *mut c_void;
            fn CGEventGetLocation(event: *mut c_void) -> Point;
            fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
            fn CGDisplayBounds(display: u32) -> CgRect;
            fn CGDisplayIsBuiltin(display: u32) -> u32;
            fn CGMainDisplayID() -> u32;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" { fn CFRelease(cf: *const c_void); }

        pub struct Display { pub rect: Rect, pub builtin: bool, pub main: bool }

        pub fn displays() -> Vec<Display> {
            let (mut ids, mut n) = ([0u32; 16], 0u32);
            if unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) } != 0 { return vec![]; }
            let main = unsafe { CGMainDisplayID() };
            ids[..n as usize].iter().map(|&id| {
                let r = unsafe { CGDisplayBounds(id) };
                Display { rect: Rect { x: r.o.x, y: r.o.y, w: r.s.w, h: r.s.h }, builtin: unsafe { CGDisplayIsBuiltin(id) } != 0, main: id == main }
            }).collect()
        }

        pub fn pointer() -> Option<(f64, f64)> {
            let e = unsafe { CGEventCreate(std::ptr::null()) };
            if e.is_null() { return None; }
            let p = unsafe { CGEventGetLocation(e) };
            unsafe { CFRelease(e) };
            Some((p.x, p.y))
        }
    }

    const POINTER_POLL_MS: u64 = 20;
    const SECURE_POLL_MS: u64 = 200;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" { fn IsSecureEventInputEnabled() -> u8; }

    /// K3: who holds Secure Input and whether the screen is locked, from the
    /// console user entry of the I/O Registry (what `ioreg` shows as
    /// IOConsoleUsers; no permission needed). CGSessionCopyCurrentDictionary is
    /// not used: on macOS 26 its kCGSSessionSecureInputPID follows the frontmost
    /// app, not the holder.
    mod session {
        use keyboard_follows_mouse::karabiner::{SecureHolder, SecureProbe};
        use std::ffi::{c_char, c_void, CStr};
        type Cf = *const c_void;
        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IORegistryGetRootEntry(main_port: u32) -> u32;
            fn IORegistryEntryCreateCFProperty(entry: u32, key: Cf, alloc: Cf, options: u32) -> Cf;
            fn IOObjectRelease(obj: u32) -> i32;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            fn CFRelease(cf: Cf);
            fn CFDictionaryGetValue(d: Cf, key: Cf) -> Cf;
            fn CFArrayGetCount(a: Cf) -> isize;
            fn CFArrayGetValueAtIndex(a: Cf, i: isize) -> Cf;
            fn CFStringCreateWithCString(alloc: Cf, s: *const c_char, encoding: u32) -> Cf;
            fn CFGetTypeID(cf: Cf) -> usize;
            fn CFArrayGetTypeID() -> usize;
            fn CFDictionaryGetTypeID() -> usize;
            fn CFBooleanGetTypeID() -> usize;
            fn CFNumberGetTypeID() -> usize;
            fn CFBooleanGetValue(b: Cf) -> u8;
            fn CFNumberGetValue(n: Cf, kind: isize, out: *mut c_void) -> u8;
        }
        const UTF8: u32 = 0x0800_0100;
        const SINT64: isize = 4;
        const LOGINWINDOW: &str = "/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow";

        fn cfstr(s: &CStr) -> Cf { unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), UTF8) } }

        /// A boolean or number value; `None` if the key is absent.
        fn int(d: Cf, key: &CStr) -> Option<i64> {
            let k = cfstr(key);
            if k.is_null() { return None; }
            let v = unsafe { CFDictionaryGetValue(d, k) };
            unsafe { CFRelease(k) };
            if v.is_null() { return None; }
            let t = unsafe { CFGetTypeID(v) };
            if t == unsafe { CFBooleanGetTypeID() } { return Some(unsafe { CFBooleanGetValue(v) } as i64); }
            let mut n = 0i64;
            (t == unsafe { CFNumberGetTypeID() } && unsafe { CFNumberGetValue(v, SINT64, (&mut n as *mut i64).cast()) } != 0).then_some(n)
        }

        fn proc_str(pid: i32, path: bool) -> Option<String> {
            let mut buf = [0u8; 4096];
            let n = unsafe { if path { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), 4096) } else { libc::proc_name(pid, buf.as_mut_ptr().cast(), 4096) } };
            (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
        }

        /// (holder pid, locked) from the on-console user; `None`s when unreadable.
        fn console() -> (Option<i32>, Option<bool>) {
            let root = unsafe { IORegistryGetRootEntry(0) };
            if root == 0 { return (None, None); }
            let k = cfstr(c"IOConsoleUsers");
            let users = if k.is_null() { std::ptr::null() } else { unsafe { IORegistryEntryCreateCFProperty(root, k, std::ptr::null(), 0) } };
            if !k.is_null() { unsafe { CFRelease(k) }; }
            unsafe { IOObjectRelease(root) };
            if users.is_null() { return (None, None); }
            let mut found = (None, None);
            if unsafe { CFGetTypeID(users) } == unsafe { CFArrayGetTypeID() } {
                for i in 0..unsafe { CFArrayGetCount(users) } {
                    let d = unsafe { CFArrayGetValueAtIndex(users, i) };
                    if d.is_null() || unsafe { CFGetTypeID(d) } != unsafe { CFDictionaryGetTypeID() } || int(d, c"kCGSSessionOnConsoleKey") != Some(1) { continue; }
                    let pid = int(d, c"kCGSSessionSecureInputPID").filter(|&p| p > 0).map(|p| p as i32);
                    // The lock key is present only while locked, so "absent" counts
                    // as unlocked only for a logged-in console session.
                    let locked = match int(d, c"CGSSessionScreenIsLocked") {
                        Some(v) => Some(v != 0),
                        None if int(d, c"kCGSessionLoginDoneKey") == Some(1) => Some(false),
                        None => None,
                    };
                    found = (pid, locked);
                    break;
                }
            }
            unsafe { CFRelease(users) };
            found
        }

        pub fn probe(enabled: bool) -> SecureProbe {
            if !enabled { return SecureProbe { enabled, holder: None, locked: None }; }
            let (pid, locked) = console();
            let holder = pid.map(|pid| SecureHolder {
                pid,
                name: proc_str(pid, false).unwrap_or_else(|| "?".into()),
                loginwindow: proc_str(pid, true).as_deref() == Some(LOGINWINDOW),
            });
            SecureProbe { enabled, holder, locked }
        }
    }

    /// Stage 7b preconditions, checked before any socket exists.
    fn check_follow(args: &[String]) -> Result<bool, String> {
        if !flag(args, "--flow-follow") { return Ok(false); }
        for (f, why) in [("--apply-route", "it moves the real route"), ("--use-karabiner-endpoint", "it follows real keyboard input"), ("--peer", "it needs the Windows receiver")] {
            if !flag(args, f) && arg(args, f).is_none() { return Err(format!("--flow-follow needs {f} ({why})")); }
        }
        if flag(args, "--resident") {
            // No timer: the guard job (route 0 whenever kfm-mac is not running) replaces it.
            let job = format!("gui/{}/local.kfm-guard", unsafe { libc::getuid() });
            let ok = std::process::Command::new("launchctl").args(["print", &job]).output().map_err(|e| e.to_string())?.status.success();
            if !ok { return Err("--resident needs the launchd guard (local.kfm-guard) loaded; see scripts/resident/install.sh".into()); }
            return Ok(true);
        }
        let job = format!("gui/{}/local.kfm-failsafe", unsafe { libc::getuid() });
        let out = std::process::Command::new("launchctl").args(["print", &job]).output().map_err(|e| e.to_string())?;
        if !String::from_utf8_lossy(&out.stdout).lines().any(|l| l.trim_start().starts_with("pid =")) {
            return Err("--flow-follow needs a fail-safe that resets kfm_route: run with --resident under scripts/resident/install.sh (its guard job), or keep your own launchd job local.kfm-failsafe running".into());
        }
        Ok(true)
    }

    struct FlowObs { displays: Vec<Rect>, candidates: Vec<usize>, edge: Edge, tracker: PointerTracker, obs: Observer, last_poll: u64 }

    impl FlowObs {
        fn from_args(args: &[String]) -> Result<Option<FlowObs>, String> {
            if !flag(args, "--flow-observe") && !flag(args, "--flow-follow") { return Ok(None); }
            let edge: Edge = arg(args, "--flow-edge").ok_or("--flow-observe needs --flow-edge left|right|top|bottom (no default)")?.parse()?;
            let ds = cg::displays();
            if ds.is_empty() { return Err("no active display found".into()); }
            let rects: Vec<Rect> = ds.iter().map(|d| d.rect).collect();
            let candidates: Result<Vec<usize>, String> = match arg(args, "--flow-display").as_deref() {
                None if ds.len() == 1 => Ok(vec![0]),
                None => Err(format!("{} displays: pass --flow-display all or N[,M] (1-based, as listed above)", ds.len())),
                Some("all") => Ok((0..ds.len()).collect()),
                Some(list) => list.split(',').map(|n| n.trim().parse::<usize>().ok().filter(|&n| n >= 1 && n <= ds.len()).map(|n| n - 1)
                    .ok_or(format!("--flow-display: {n:?} is not 1..{}", ds.len()))).collect(),
            };
            // The display list is printed even when the choice is missing, so it can be made.
            let chosen = candidates.as_ref().map(Vec::as_slice).unwrap_or(&[]);
            println!("flow 7a (observe only; it never changes the route): edge {edge:?}; {} display(s):", ds.len());
            for (i, d) in ds.iter().enumerate() {
                let outer = keyboard_follows_mouse::flow::outer_edge_share(&rects, i, edge);
                println!("  display {}: {}{}, {}x{} points, {edge:?} edge facing outward: {}, candidate: {}", i + 1,
                    if d.builtin { "built-in" } else { "external" }, if d.main { " (main)" } else { "" }, d.rect.w, d.rect.h,
                    match outer { 0 => "no", 100 => "yes", _ => "partly" }, if chosen.contains(&i) { "yes" } else { "no" });
            }
            let candidates = candidates?;
            let obs = Observer::new(FlowConfig::default(), ds.len());
            Ok(Some(FlowObs { displays: rects, candidates, edge, tracker: PointerTracker::default(), obs, last_poll: 0 }))
        }

        fn poll(&mut self, now: u64) -> Vec<Note> {
            if now.saturating_sub(self.last_poll) < POINTER_POLL_MS { return vec![]; }
            self.last_poll = now;
            let mut notes = vec![];
            if let Some((x, y)) = cg::pointer()
                && let Some(ev) = self.tracker.on_sample(&self.displays, &self.candidates, self.edge, x, y) {
                notes.extend(self.obs.on_pointer(ev, now, self.tracker.from_edge_pt()));
            }
            notes.extend(self.obs.tick(now));
            notes
        }

        fn summary(&self) {
            let c = &self.obs.counts;
            let per = |v: &[u64]| v.iter().enumerate().map(|(i, n)| format!("d{}={n}", i + 1)).collect::<Vec<_>>().join(" ");
            let dec = |d: &Decisions| format!("would switch to Windows [{}]; back to Mac, pointer on [{}] unknown={}", per(&d.to_windows), per(&d.to_mac), d.to_mac_unknown_display);
            println!("flow 7a summary: edges [{}] (rule A: expired {}, cancelled by a Mac move {}); windows activity bursts {} (ignored by rule A: {}); windows reports {} (mean gap inside a burst {} ms); windows left edge: entered {}, departed {}, reached {}",
                per(&c.edges), c.edges_expired, c.edges_cancelled, c.win_bursts, c.win_bursts_ignored, c.win_reports,
                c.win_gap_sum_ms.checked_div(c.win_gaps).map_or("?".into(), |m| m.to_string()), c.win_entered, c.win_departed, c.win_reached);
            println!("flow 7a summary rule A: {}", dec(&c.a));
            println!("flow 7a summary rule C: {}; Mac moves held {}", dec(&c.c), c.c_mac_moves_held);
        }
    }

    fn opt(v: Option<u64>) -> String { v.map_or("?".into(), |m| m.to_string()) }
    fn disp(d: Option<usize>) -> String { d.map_or("?".into(), |d| (d + 1).to_string()) }
    fn tag(r: Rule) -> &'static str { match r { Rule::A => "A", Rule::C => "C" } }

    fn log_notes(notes: &[Note], now: u64) {
        for n in notes {
            let t = format!("[flow +{}.{:03}s]", now / 1000, now % 1000);
            match n {
                Note::Edge { display } => println!("{t} edge reached on display {}", display + 1),
                Note::EdgeExpired { display } => println!("{t} [A] edge on display {} saw no windows activity in time", display + 1),
                Note::WinBurst { ignored, since_edge_ms } => println!("{t} windows mouse activity{} (last edge {} ms ago)",
                    if *ignored { ": [A] ignored (misjudgment candidate if Flow did move)" } else { "" }, opt(*since_edge_ms)),
                Note::ToWindows { rule, display, after_edge_ms } => println!("{t} [{}] WOULD SWITCH to Windows (edge on display {}, {} ms after the edge)",
                    tag(*rule), disp(*display), opt(*after_edge_ms)),
                Note::ToMac { rule, display, after_windows_ms, from_edge_pt } => println!("{t} [{}] WOULD SWITCH back to Mac (pointer moved on display {}, {} ms after the switch, {} pt from the edge)",
                    tag(*rule), disp(*display), opt(*after_windows_ms), opt(from_edge_pt.map(u64::from))),
                Note::WinEdge { event, since_edge_ms } => println!("{t} windows left edge: {} (last Mac edge {} ms ago)",
                    match event { WinEdgeEvent::Entered => "entered (jumped in)", WinEdgeEvent::Departed => "departed inward", WinEdgeEvent::Reached => "reached (heading to the Mac)" }, opt(*since_edge_ms)),
                Note::MacMoveHeld { after_windows_ms, from_edge_pt } => println!("{t} [C] kept Windows despite a Mac move ({after_windows_ms} ms after the switch, {} pt from the edge)", opt(from_edge_pt.map(u64::from))),
                Note::LinkDown => println!("{t} link down: pending edge dropped (keys follow the Hold policy, not Flow)"),
            }
        }
    }

    /// Returns how long karabiner_cli took (ms); the loop waits for it.
    fn set_route(remote: bool, apply: bool) -> u64 {
        let json = format!("{{\"kfm_route\":{}}}", u8::from(remote));
        if !apply { println!("[dry-run] would run: karabiner_cli --set-variables '{json}'"); return 0; }
        let t = Instant::now();
        let res = std::process::Command::new(KARABINER_CLI).args(["--set-variables", &json]).status();
        let ms = t.elapsed().as_millis() as u64;
        match res {
            Ok(s) if s.success() => println!("route set: {json} ({ms} ms)"),
            other => eprintln!("karabiner_cli failed ({other:?}). {}", recovery()),
        }
        ms
    }

    /// The Windows modifier map: with the Karabiner endpoint, the keyboard's simple
    /// modifications are undone for Windows (read from karabiner.json, read-only);
    /// `--win-modmap off` or a dry run sends keys as Karabiner reports them. A
    /// read failure warns and sends them unchanged.
    fn win_profile(args: &[String]) -> Result<WindowsProfile, String> {
        let mode = arg(args, "--win-modmap").unwrap_or_else(|| if flag(args, "--use-karabiner-endpoint") { "karabiner".into() } else { "off".into() });
        match mode.as_str() {
            "off" => { println!("windows modifier map: off (keys go to Windows as Karabiner reports them)"); return Ok(WindowsProfile::identity()); }
            "karabiner" => {}
            m => return Err(format!("--win-modmap {m:?}: use karabiner or off")),
        }
        let path = arg(args, "--karabiner-json").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/karabiner/karabiner.json"));
        let read = std::fs::read(&path).map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
            .and_then(|v| keyboard_follows_mouse::karabiner_config::windows_modifier_map(&v));
        match read {
            Ok(m) => {
                let name = |u| keyboard_follows_mouse::karabiner::name_of(u).unwrap_or("?");
                let list = m.map.iter().map(|&(r, p)| format!("reported {} = physical {}", name(r), name(p))).collect::<Vec<_>>().join(", ");
                println!("windows modifier map (Windows gets the physical key; the Mac keeps Karabiner's swap): {}", if list.is_empty() { "nothing to undo".into() } else { list });
                for n in m.notes { println!("  note: {n}"); }
                for w in m.warnings { println!("WARNING: windows modifier map: {w}"); }
                Ok(WindowsProfile::undo_swaps(&m.map))
            }
            Err(e) => {
                println!("WARNING: windows modifier map not read ({e}); keys go to Windows as Karabiner reports them");
                Ok(WindowsProfile::identity())
            }
        }
    }

    /// `--link-timeout-ms` (default 300): how long without a fresh Ack (or with
    /// keys unapplied) before the link counts as down and keys are Held.
    fn timing(args: &[String]) -> Result<Timing, String> {
        let mut t = Timing::default();
        if let Some(v) = arg(args, "--link-timeout-ms") {
            t.link_timeout_ms = v.parse().ok().filter(|ms| (100..=2000).contains(ms)).ok_or("--link-timeout-ms must be 100..2000")?;
            println!("link timeout: {} ms (default {})", t.link_timeout_ms, Timing::default().link_timeout_ms);
        }
        Ok(t)
    }

    /// Resident restart after a crash: our old socket file may remain. Remove it
    /// only if it is a socket nobody receives on (a send is refused).
    fn clear_stale_socket(p: &Path) -> Result<(), String> {
        use std::os::unix::fs::FileTypeExt;
        let Ok(m) = std::fs::symlink_metadata(p) else { return Ok(()) };
        if !m.file_type().is_socket() { return Err(format!("{} exists and is not a socket; not touching it", p.display())); }
        let probe = UnixDatagram::unbound().map_err(|e| e.to_string())?;
        match probe.send_to(b"{}", p) {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(p).map_err(|e| e.to_string())?;
                println!("resident: removed a stale receiver socket (nobody was receiving)");
                Ok(())
            }
            _ => Err(format!("{} is in use by another receiver; not starting", p.display())),
        }
    }

    fn link_log(change: LinkChange, net: &Net, now: u64, pause_max: u64, cli: Option<(u64, u64)>) {
        let t = format!("[link +{}.{:03}s]", now / 1000, now % 1000);
        if change == LinkChange::Up { println!("{t} up"); return; }
        let (ack, unapplied) = net.ages(now);
        let ms = |v: Option<u64>| v.map_or("none".into(), |v| format!("{v} ms"));
        let cli = cli.map_or("none yet".into(), |(at, d)| format!("took {d} ms, {} ms ago", now.saturating_sub(at)));
        println!("{t} down: last Ack {} ago; keys waiting to be applied: {}; longest loop pause (2 s) {pause_max} ms; last karabiner_cli {cli}", ms(ack), ms(unapplied));
    }

    fn run(args: &[String]) -> Result<(), String> {
        let apply_route = flag(args, "--apply-route");
        // Argument errors first, before any socket exists.
        let follow = check_follow(args)?;
        let mut flow = FlowObs::from_args(args)?;
        let (mut secure, mut last_secure_poll, mut verdict, mut stuck_logged) = (false, 0u64, SecureVerdict::Off, false);
        // Socket: a fresh private dir by default; the Karabiner endpoint only on request.
        let (sock_path, created_dir) = if let Some(p) = arg(args, "--socket-path") {
            if !flag(args, "--use-karabiner-endpoint") { return Err("--socket-path needs --use-karabiner-endpoint (this listens where Karabiner sends)".into()); }
            let p = PathBuf::from(p);
            check_dir(p.parent().ok_or("bad socket path")?)?;
            if flag(args, "--resident") { clear_stale_socket(&p)?; }
            (p, None)
        } else {
            let dir = arg(args, "--socket-dir").map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join(format!("kfm-dryrun-{}", std::process::id())));
            std::fs::create_dir(&dir).map_err(|e| format!("{}: {e} (pass a new directory)", dir.display()))?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
            (dir.join("user_command_receiver.sock"), Some(dir))
        };
        let rx = UnixDatagram::bind(&sock_path).map_err(|e| {
            if let Some(d) = &created_dir { let _ = std::fs::remove_dir(d); }
            format!("bind {}: {e} (UNIX socket paths must be under ~104 bytes; use a shorter --socket-dir)", sock_path.display())
        })?;
        let rcvbuf = set_rcvbuf(&rx);
        // K6: if Karabiner is restarted it may remove or replace this socket; then
        // nothing more can arrive, so return to the Mac and stop (checked each second).
        let sock_id = std::fs::metadata(&sock_path).map(|m| (m.dev(), m.ino())).map_err(|e| e.to_string())?;
        rx.set_read_timeout(Some(Duration::from_millis(5))).unwrap();

        let mut net = if flag(args, "--loopback") {
            let key = [7u8; 32]; // in-process only, never leaves this process
            Net::Loopback { link: MacLink::new(key, Timing::default()), win: WinLink::new(key, Timing::default()), inj: CountingInjector::default() }
        } else if let Some(peer) = arg(args, "--peer") {
            if !flag(args, "--enable-send") { return Err("--peer needs --enable-send (sends key events over the LAN)".into()); }
            let key = keyfile::load(Path::new(&arg(args, "--key-file").ok_or("--key-file is required with --peer")?))?;
            let sock = UdpSocket::bind(arg(args, "--bind").ok_or("--bind IP:PORT is required with --peer")?).map_err(|e| e.to_string())?;
            sock.set_nonblocking(true).unwrap();
            Net::Udp { link: MacLink::new(key, timing(args)?), sock, peer: peer.parse().map_err(|e| format!("--peer: {e}"))? }
        } else { Net::None };

        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            // An interactive zsh starts background jobs with SIGINT blocked; a handler
            // alone would never run, so `kill -INT` would not stop us.
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        }
        std::panic::set_hook(Box::new(move |info| {
            eprintln!("kfm-mac panicked: {info}\n{}", recovery());
            if apply_route { let _ = std::process::Command::new(KARABINER_CLI).args(["--set-variables", "{\"kfm_route\":0}"]).status(); }
        }));

        let mode = match net { Net::None => "no link (Windows will be refused)", Net::Loopback { .. } => "loopback (in-process receiver, counts only)", Net::Udp { .. } => "UDP to the pinned peer" };
        println!("kfm-mac: socket {} (SO_RCVBUF {rcvbuf}); link: {mode}; route changes: {}", sock_path.display(), if apply_route { "APPLIED via karabiner_cli" } else { "dry-run" });
        println!("{}", recovery());

        if flow.is_some() && !matches!(net, Net::Udp { .. }) { println!("flow 7a: no Windows receiver (--peer), so Windows activity cannot be observed"); }
        if follow { println!("flow 7b: FOLLOWING Flow with rule C (route changes applied). Fn+F12 overrides until the next Flow decision; Secure Input keeps keys on the Mac."); }
        let easy_switch = !flag(args, "--no-easy-switch");
        if follow { println!("rule E: {}", if easy_switch { "FOLLOWING the MX4 (Windows device list: arrives -> Windows, leaves -> Mac; Easy-Switch and Flow)" } else { "off (--no-easy-switch)" }); }
        let mut mx4_seen = [0u64; 2];
        let start = Instant::now();
        let mut bridge = Bridge::new(Router::new(win_profile(args)?, Hotkey::default()), Typematic::default());
        let (mut events, mut rejected, mut dropped) = (0u64, 0u64, 0u64);
        let mut buf = [0u8; 512];
        // Link diagnosis: the longest pause between loop turns (over ~2 s) and the last karabiner_cli run.
        let (mut prev_now, mut pause_max, mut pause_at, mut cli) = (0u64, 0u64, 0u64, None::<(u64, u64)>);
        let (mut sent_down, mut sent_repeat, mut sent_up) = (0u64, 0u64, 0u64);
        // Stage 7c run summary: destination changes by (dest, reason), link downs, Secure Input verdicts.
        let mut dests: std::collections::BTreeMap<String, u64> = Default::default();
        let (mut downs, mut down_ack_max, mut down_pause_max) = (0u64, 0u64, 0u64);
        let mut verdicts: std::collections::BTreeMap<String, u64> = Default::default();
        let (mut last_sock_check, mut last_alive, mut socket_lost) = (0u64, 0u64, false);
        // Resident safety: start from route 0; a Hold that lasts too long gives the
        // keyboard back to the Mac; a pause file keeps everything on the Mac.
        let resident = flag(args, "--resident");
        let hold_fallback_ms: Option<u64> = match arg(args, "--hold-fallback-ms") {
            Some(v) => Some(v.parse().ok().filter(|ms| (500..=600_000).contains(ms)).ok_or("--hold-fallback-ms must be 500..600000")?),
            None => resident.then_some(5000),
        };
        let pause_file: Option<PathBuf> = arg(args, "--pause-file").map(PathBuf::from)
            .or_else(|| resident.then(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/keyboard-follows-mouse/paused")));
        let (mut held_since, mut paused, mut secure_hold) = (None::<u64>, false, false);
        if apply_route { set_route(false, true); println!("route 0 at start"); }
        if resident { println!("resident: Hold falls back to the Mac after {} ms; pause file {}", hold_fallback_ms.unwrap_or(0), pause_file.as_ref().map_or("none".into(), |p| p.display().to_string())); }
        while !STOP.load(Ordering::SeqCst) {
            let now = start.elapsed().as_millis() as u64;
            let pause = now.saturating_sub(prev_now);
            prev_now = now;
            if pause >= pause_max || now.saturating_sub(pause_at) > 2000 { (pause_max, pause_at) = (pause, now); }
            let mut outs = Vec::new();
            match rx.recv(&mut buf) {
                Ok(n) => match parse(&buf[..n]) {
                    Ok(ev) => { events += 1; outs.extend(bridge.on_input(ev, now)); }
                    Err(e) => { rejected += 1; eprintln!("rejected datagram ({e:?})"); }
                },
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => return Err(format!("recv: {e}")),
            }
            outs.extend(bridge.tick(now));
            if now.saturating_sub(last_sock_check) >= 1000 {
                last_sock_check = now;
                if std::fs::metadata(&sock_path).map(|m| (m.dev(), m.ino())).ok() != Some(sock_id) {
                    println!("[k6 +{}.{:03}s] receiver socket removed or replaced (Karabiner restarted?): back to the Mac and stopping", now / 1000, now % 1000);
                    socket_lost = true;
                    STOP.store(true, Ordering::SeqCst);
                    outs.extend(bridge.on_input(keyboard_follows_mouse::karabiner::BridgeIn::Escape, now));
                }
                if let Some(pf) = &pause_file && pf.exists() != paused {
                    paused = !paused;
                    println!("[pause +{}.{:03}s] {}", now / 1000, now % 1000, if paused { "pause file present: keys stay on the Mac" } else { "pause file removed: following again (from the next decision)" });
                }
            }
            if let Some(limit) = hold_fallback_ms {
                if bridge.dest() == keyboard_follows_mouse::router::Dest::Held {
                    let since = *held_since.get_or_insert(now);
                    if now.saturating_sub(since) >= limit {
                        println!("[hold +{}.{:03}s] link down for {limit} ms while on Windows: back to the Mac", now / 1000, now % 1000);
                        outs.extend(bridge.fallback_to_mac());
                        held_since = None;
                    }
                } else { held_since = None; }
            }
            if now.saturating_sub(last_alive) >= 300_000 {
                last_alive = now;
                println!("[alive +{}s] link {}, destination {:?}", now / 1000, if net.is_up() { "up" } else { "down" }, bridge.dest());
            }
            for change in net.poll(now) {
                link_log(change, &net, now, pause_max, cli);
                if change == LinkChange::Down {
                    downs += 1;
                    down_pause_max = down_pause_max.max(pause_max);
                    if let Some(a) = net.ages(now).0 { down_ack_max = down_ack_max.max(a); }
                }
                if change == LinkChange::Down && let Some(f) = flow.as_mut() { log_notes(&f.obs.on_link_down(now), now); }
                outs.extend(bridge.on_link(change == LinkChange::Up));
            }
            let p = net.take_peer();
            // Rule E: the MX4 arrived on / left the Windows PC (Easy-Switch, or Flow
            // handing it over). Logged always; moves the route only when following.
            if let Some(target) = rule_e(p.mx4_arrivals, p.mx4_departures, p.mx4_present) {
                mx4_seen[0] += u64::from(p.mx4_arrivals);
                mx4_seen[1] += u64::from(p.mx4_departures);
                let act = follow && easy_switch;
                println!("[mx4 +{}.{:03}s] MX4 on Windows: arrived {}, left {}, now {}: {} {target:?}", now / 1000, now % 1000, p.mx4_arrivals, p.mx4_departures,
                    if p.mx4_present { "present" } else { "absent" }, if act { "switch to" } else { "WOULD SWITCH to" });
                if act { outs.extend(bridge.on_easy_switch(target)); }
            }
            if let Some(f) = flow.as_mut() {
                let mut notes = f.poll(now);
                for (n, e) in [(p.entered, WinEdgeEvent::Entered), (p.departed, WinEdgeEvent::Departed), (p.reached, WinEdgeEvent::Reached)] {
                    for _ in 0..n.min(8) { notes.extend(f.obs.on_win_edge(e, now)); }
                }
                if p.moved { notes.extend(f.obs.on_win_activity(now)); }
                log_notes(&notes, now);
                if follow {
                    for n in &notes {
                        match n {
                            Note::ToWindows { rule: Rule::C, .. } => outs.extend(bridge.on_flow(Target::Windows)),
                            Note::ToMac { rule: Rule::C, .. } => outs.extend(bridge.on_flow(Target::Mac)),
                            _ => {}
                        }
                    }
                }
            }
            if follow && now.saturating_sub(last_secure_poll) >= SECURE_POLL_MS {
                last_secure_poll = now;
                let p = session::probe(unsafe { IsSecureEventInputEnabled() } != 0);
                let v = secure_verdict(&p);
                if v != verdict {
                    verdict = v;
                    *verdicts.entry(format!("{v:?}")).or_default() += 1;
                    let t = format!("[flow +{}.{:03}s]", now / 1000, now % 1000);
                    let who = p.holder.as_ref().map_or("holder unknown".into(), |h| format!("held by {} (pid {})", h.name, h.pid));
                    let lock = match p.locked { Some(true) => "screen locked", Some(false) => "screen unlocked", None => "lock state unknown" };
                    match v {
                        SecureVerdict::Off => println!("{t} macOS Secure Input off"),
                        SecureVerdict::Hold => println!("{t} macOS Secure Input ON ({who}; {lock}): keys stay on the Mac"),
                        SecureVerdict::StuckLoginwindow if !stuck_logged => {
                            stuck_logged = true;
                            println!("{t} Secure Input held by loginwindow (pid {}) while unlocked: ignored. While it stays held, K3 cannot see other apps' password fields (the holder stays loginwindow); logging out clears it (lock and unlock may too; untested)", p.holder.as_ref().map_or(0, |h| h.pid));
                        }
                        SecureVerdict::StuckLoginwindow => {}
                    }
                }
                secure_hold = verdict == SecureVerdict::Hold;
            }
            // Secure Input and the pause file both keep keys on the Mac (same gate).
            let s = secure_hold || paused;
            if s != secure {
                secure = s;
                outs.extend(bridge.on_secure_input(s));
            }
            let mut i = 0;
            while i < outs.len() {
                match outs[i] {
                    Out::Remote { key, down, repeat } => {
                        match (down, repeat) { (true, true) => sent_repeat += 1, (true, false) => sent_down += 1, (false, _) => sent_up += 1 }
                        for change in net.send_key(key, down, repeat, now) { link_log(change, &net, now, pause_max, cli); outs.extend(bridge.on_link(change == LinkChange::Up)); }
                    }
                    Out::SetRoute { remote } => { let t = start.elapsed().as_millis() as u64; cli = Some((t, set_route(remote, apply_route))); }
                    Out::Dest { dest, reason } => { *dests.entry(format!("{dest:?} ({reason:?})")).or_default() += 1; println!("destination: {dest:?} ({reason:?})") }
                    Out::Refused => println!("switch to Windows refused: link not confirmed"),
                    Out::MacKeyDropped => { dropped += 1; println!("warning: a key meant for the Mac was swallowed (total {dropped}); if a key seems stuck on the Mac, press and release it") }
                }
                i += 1;
            }
        }
        if bridge.karabiner_remote() { set_route(false, apply_route); }
        if let Net::Loopback { inj, .. } = &net { println!("loopback receiver injected: {} down / {} up", inj.downs, inj.ups); }
        println!("stopped: {events} events, {rejected} rejected, {dropped} Mac-bound keys swallowed");
        if matches!(net, Net::Udp { .. } | Net::Loopback { .. }) { println!("sent to Windows: {sent_down} down + {sent_repeat} auto-repeat / {sent_up} up; key presses swallowed while Held (link down): {}", bridge.held_drops()); }
        if let Some(f) = &flow { f.summary(); }
        let list = |m: &std::collections::BTreeMap<String, u64>| if m.is_empty() { "none".to_string() } else { m.iter().map(|(k, n)| format!("{k}={n}")).collect::<Vec<_>>().join(", ") };
        println!("run summary: {} s; destination changes: {}; link downs {downs} (longest Ack gap {down_ack_max} ms, longest loop pause {down_pause_max} ms); Secure Input verdicts: {}; MX4 on Windows: arrived {}, left {}{}",
            start.elapsed().as_secs(), list(&dests), list(&verdicts), mx4_seen[0], mx4_seen[1], if socket_lost { "; stopped because the receiver socket went away (K6)" } else { "" });
        // Only our own socket: after a K6 restart the path may belong to something else.
        if std::fs::metadata(&sock_path).map(|m| (m.dev(), m.ino())).ok() == Some(sock_id) { let _ = std::fs::remove_file(&sock_path); }
        if let Some(d) = created_dir { let _ = std::fs::remove_dir(d); }
        println!("{}", recovery());
        // A non-zero exit lets launchd start a fresh receiver on the new socket.
        if socket_lost { return Err("receiver socket went away (K6); exiting with an error so a resident job restarts".into()); }
        Ok(())
    }

    fn send_test(args: &[String]) -> Result<(), String> {
        let sock = arg(args, "--socket").ok_or("--socket PATH is required")?;
        let tx = UnixDatagram::unbound().map_err(|e| e.to_string())?;
        let script: &[&str] = &[
            r#"{"kfm":1,"e":"toggle"}"#,
            r#"{"kfm":1,"k":"left_shift","d":1}"#, r#"{"kfm":1,"k":"a","d":1}"#,
            r#"{"kfm":1,"k":"a","d":0}"#, r#"{"kfm":1,"k":"left_shift","d":0}"#,
            r#"{"kfm":1,"k":"international3","d":1}"#,
            r#"{"kfm":1,"e":"escape"}"#,
        ];
        for m in script {
            tx.send_to(m.as_bytes(), &sock).map_err(|e| format!("send: {e}"))?;
            std::thread::sleep(Duration::from_millis(150));
        }
        println!("sent {} synthetic datagrams (toggle, Shift+A, an unsupported JIS key, escape)", script.len());
        Ok(())
    }

    pub fn main() -> ExitCode {
        if let Err(e) = keyboard_follows_mouse::karabiner::keyboard_from_env() { eprintln!("kfm-mac: {e}"); return ExitCode::FAILURE; }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let res = match args.first().map(String::as_str) {
            Some("keygen") => arg(&args, "--out").ok_or("--out PATH is required".to_string())
                .and_then(|p| keyfile::generate(Path::new(&p)).map_err(|e| e.to_string()).map(|_| println!("key written (owner-only): {p}"))),
            Some("run") => run(&args[1..]),
            Some("send-test") => send_test(&args[1..]),
            Some("secure-probe") => {
                let p = session::probe(unsafe { IsSecureEventInputEnabled() } != 0);
                println!("secure probe: {p:?} -> {:?}", secure_verdict(&p));
                Ok(())
            }
            _ => Err("usage: kfm-mac keygen|run|send-test|secure-probe (see source header)".into()),
        };
        match res { Ok(()) => ExitCode::SUCCESS, Err(e) => { eprintln!("kfm-mac: {e}"); ExitCode::FAILURE } }
    }
}
