//! EXPERIMENTAL Windows source: a Windows keyboard drives a Mac running
//! `kfm-receive`. Windows can suppress a key only through a global hook, which
//! cannot tell keyboards apart, so while keys are forwarded EVERY keyboard on
//! this PC (a laptop's built-in one included) goes to the Mac. At rest there
//! is no hook at all and every key stays on this PC.
//!
//!   kfm-send --peer IP:PORT --bind IP:PORT --key-file PATH
//!            [--experimental-capture-all-keyboards]
//!            [--flow-edge left|right|top|bottom [--flow-display all|N[,M]]]
//!            [--link-timeout-ms N] [--link-loss-stop-ms N] [--idle-stop-secs N]
//!
//! Without `--experimental-capture-all-keyboards` it is a dry run: the link
//! and Flow decisions are printed (as "would capture"); no hook is installed,
//! no hotkey is registered and no key is ever suppressed.
//!
//! With it: the switch hotkey (F13..F20, `KFM_KEYMAP` as for kfm-mac) is
//! registered with RegisterHotKey. Pressing it, with the link up, installs a
//! WH_KEYBOARD_LL hook and forwards; pressing it again removes the hook. The
//! hook is also removed when Flow hands the mouse back (`--flow-edge`, same
//! rule C as kfm-mac) or, without `--flow-edge`, when the cursor on this PC
//! moves 60 px or more; when the link has been down for `--link-loss-stop-ms`
//! (default 3000; until then input is swallowed, not typed here); after
//! `--idle-stop-secs` (default 600) without a forwarded key; and when the
//! cursor cannot be read (the secure desktop has input). The keys held on the
//! Mac are released on every exit. Windows also removes the hook if this
//! process dies.
//!
//! While forwarding, Ctrl+C is captured like any other key and cannot stop the
//! program: the ways out are the hotkey, moving the mouse back, closing this
//! console window with the mouse, and the automatic stops. Ctrl+C works when
//! not forwarding.
//!
//! Key contents are never logged or printed: only state changes and counts.
//! Not covered: the secure desktop (lock screen, UAC, Ctrl+Alt+Del) never
//! reaches the hook, so keys stay local there.

#[cfg(not(windows))]
fn main() {
    eprintln!("kfm-send runs on Windows only (a Windows keyboard as the source for a Mac receiver)");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> std::process::ExitCode { win::main() }

#[cfg(windows)]
mod win {
    use std::net::{SocketAddr, UdpSocket};
    use std::path::Path;
    use std::process::ExitCode;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{mpsc, Mutex, OnceLock, PoisonError};
    use std::time::{Duration, Instant};

    use keyboard_follows_mouse::flow::{outer_edge_share, Edge, FlowConfig, Note, Observer, PointerTracker, Rect, Rule, WinEdgeEvent};
    use keyboard_follows_mouse::keymap::WindowsProfile;
    use keyboard_follows_mouse::keyfile;
    use keyboard_follows_mouse::remap::TargetOs;
    use keyboard_follows_mouse::router::{Local, Target};
    use keyboard_follows_mouse::transport::{LinkChange, MacLink, Timing};
    use keyboard_follows_mouse::winsource::{Config, Effect, LocalMouse, WinSource};

    /// Console output goes through a printer thread, so a console that blocks
    /// (QuickEdit selection, a full buffer) cannot stall the link or the auto-stops.
    enum Out { Line(String), Flush(mpsc::Sender<()>) }
    static PRINT: OnceLock<mpsc::Sender<Out>> = OnceLock::new();

    fn start_printer() {
        let (tx, rx) = mpsc::channel();
        let _ = PRINT.set(tx);
        let _ = std::thread::Builder::new().name("kfm-send-print".into()).spawn(move || {
            for o in rx {
                match o { Out::Line(l) => println!("{l}"), Out::Flush(done) => { let _ = done.send(()); } }
            }
        });
    }
    fn say(line: String) {
        match PRINT.get() { Some(tx) => { let _ = tx.send(Out::Line(line)); } None => println!("{line}") }
    }
    /// Waits (at most 1 s) for what was said so far to be printed.
    fn flush() {
        if let Some(tx) = PRINT.get() {
            let (done, wait) = mpsc::channel();
            if tx.send(Out::Flush(done)).is_ok() { let _ = wait.recv_timeout(Duration::from_secs(1)); }
        }
    }
    macro_rules! say { ($($a:tt)*) => { say(format!($($a)*)) } }

    /// Events marked with this dwExtraInfo are kfm-receive's own SendInput
    /// events; keep it equal to `sendinput::MARK` in kfm-receive.rs.
    const MARK: usize = 0x5237_3500;
    const HOTKEY_ID: i32 = 1;
    const CURSOR_POLL_MS: u64 = 20;
    /// Without Flow: the cursor on this PC moving this far from where capture began means the mouse is back.
    const MOUSE_RETURN_PX: i32 = 60;

    mod sys {
        pub type HookProc = unsafe extern "system" fn(i32, usize, isize) -> isize;
        #[repr(C)]
        pub struct KbdLlHookStruct { pub vk: u32, pub scan: u32, pub flags: u32, pub time: u32, pub extra: usize }
        #[repr(C)]
        pub struct Point { pub x: i32, pub y: i32 }
        #[repr(C)]
        pub struct Msg { pub hwnd: isize, pub message: u32, pub wparam: usize, pub lparam: isize, pub time: u32, pub pt: Point }
        #[repr(C)]
        pub struct RectI { pub l: i32, pub t: i32, pub r: i32, pub b: i32 }
        #[repr(C)]
        pub struct MonitorInfo { pub size: u32, pub monitor: RectI, pub work: RectI, pub flags: u32 }
        pub type EnumProc = unsafe extern "system" fn(isize, isize, *mut RectI, isize) -> i32;

        pub const WH_KEYBOARD_LL: i32 = 13;
        pub const HC_ACTION: i32 = 0;
        pub const LLKHF_EXTENDED: u32 = 0x01;
        pub const LLKHF_LOWER_IL_INJECTED: u32 = 0x02;
        pub const LLKHF_INJECTED: u32 = 0x10;
        pub const WM_QUIT: u32 = 0x0012;
        pub const WM_HOTKEY: u32 = 0x0312;
        pub const WM_APP: u32 = 0x8000;
        pub const WM_USER: u32 = 0x0400;
        pub const WM_KEYDOWN: u32 = 0x0100;
        pub const WM_KEYUP: u32 = 0x0101;
        pub const WM_SYSKEYDOWN: u32 = 0x0104;
        pub const WM_SYSKEYUP: u32 = 0x0105;
        pub const PM_NOREMOVE: u32 = 0;
        pub const MOD_NOREPEAT: u32 = 0x4000;

        #[link(name = "user32")]
        unsafe extern "system" {
            pub fn SetWindowsHookExW(id: i32, proc: HookProc, module: isize, thread: u32) -> isize;
            pub fn UnhookWindowsHookEx(hook: isize) -> i32;
            pub fn CallNextHookEx(hook: isize, code: i32, wparam: usize, lparam: isize) -> isize;
            pub fn RegisterHotKey(hwnd: isize, id: i32, modifiers: u32, vk: u32) -> i32;
            pub fn UnregisterHotKey(hwnd: isize, id: i32) -> i32;
            pub fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
            pub fn PeekMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32, remove: u32) -> i32;
            pub fn PostThreadMessageW(thread: u32, msg: u32, wparam: usize, lparam: isize) -> i32;
            pub fn GetAsyncKeyState(vk: i32) -> i16;
            pub fn GetCursorPos(p: *mut Point) -> i32;
            pub fn SetProcessDPIAware() -> i32;
            pub fn EnumDisplayMonitors(hdc: isize, clip: *const RectI, cb: EnumProc, data: isize) -> i32;
            pub fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            pub fn GetModuleHandleW(name: *const u16) -> isize;
            pub fn GetCurrentThreadId() -> u32;
            pub fn SetConsoleCtrlHandler(h: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
        }
    }
    use sys::*;

    /// State shared by the hook (on the UI thread) and the network thread. The
    /// hook only decides, under this lock, and queues effects; it does no I/O.
    struct Shared {
        ws: Mutex<WinSource>,
        tx: mpsc::Sender<Effect>,
        /// The UI thread that owns the hotkey and the hook (0 = none: dry run).
        tid: AtomicU32,
        start: Instant,
    }
    static SHARED: OnceLock<Shared> = OnceLock::new();
    static STOP: AtomicBool = AtomicBool::new(false);

    impl Shared {
        fn now(&self) -> u64 { self.start.elapsed().as_millis() as u64 }

        /// Runs `f` on the state. Its effects are queued while the lock is
        /// held, so they reach the network thread in the order they were
        /// decided; the UI thread is woken when the hook should come or go.
        fn with<R>(&self, f: impl FnOnce(&mut WinSource) -> (R, Vec<Effect>)) -> R {
            let mut ws = self.ws.lock().unwrap_or_else(PoisonError::into_inner);
            let before = ws.wants_hook();
            let (r, fx) = f(&mut ws);
            for e in fx { let _ = self.tx.send(e); }
            let changed = ws.wants_hook() != before;
            drop(ws);
            if changed { self.kick(); }
            r
        }

        fn wants_hook(&self) -> bool { self.ws.lock().unwrap_or_else(PoisonError::into_inner).wants_hook() }

        fn kick(&self) {
            let tid = self.tid.load(Ordering::SeqCst);
            if tid != 0 { unsafe { PostThreadMessageW(tid, WM_APP, 0, 0) }; }
        }
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
        if code == HC_ACTION && lparam != 0 && let Some(sh) = SHARED.get() {
            let down = match wparam as u32 { WM_KEYDOWN | WM_SYSKEYDOWN => Some(true), WM_KEYUP | WM_SYSKEYUP => Some(false), _ => None };
            if let Some(down) = down {
                let k = unsafe { &*(lparam as *const KbdLlHookStruct) };
                let injected = k.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0 || k.extra == MARK;
                let local = sh.with(|ws| {
                    let r = ws.on_hook_key(k.vk, k.scan, k.flags & LLKHF_EXTENDED != 0, injected, down, sh.now());
                    (r.local, r.effects)
                });
                if local == Local::Suppress { return 1; }
            }
        }
        unsafe { CallNextHookEx(0, code, wparam, lparam) }
    }

    /// Every virtual key down right now (state only, nothing is recorded; mouse buttons
    /// and the generic Shift/Ctrl/Alt, which the hook never reports, are skipped).
    fn down_keys() -> Vec<u32> {
        (0x07u32..=0xFE).filter(|vk| !(0x10..=0x12).contains(vk) && unsafe { GetAsyncKeyState(*vk as i32) } < 0).collect()
    }

    /// Installs / removes the hook to match the state. Never holds the lock
    /// across a user32 call (the hook runs on this thread, inside GetMessage).
    fn sync_hook(sh: &Shared, hook: &mut isize) {
        let wants = sh.wants_hook();
        if wants && *hook == 0 {
            *hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, hook_proc, GetModuleHandleW(std::ptr::null()), 0) };
            if *hook == 0 {
                say!("kfm-send: could not install the keyboard hook; staying local");
                sh.with(|ws| ((), ws.on_hook_failed()));
            } else {
                // Keys that went up since the snapshot (the hotkey included) will never report it.
                let down = down_keys();
                sh.with(|ws| { ws.on_hook_installed(&down); ((), vec![]) });
            }
        } else if !wants && *hook != 0 {
            unsafe { UnhookWindowsHookEx(*hook) };
            *hook = 0;
        }
    }

    /// Owns the hotkey and the hook: both need a message loop on one thread.
    fn ui_thread(ready: mpsc::Sender<Result<(), String>>) {
        let Some(sh) = SHARED.get() else { return };
        let vk = sh.ws.lock().unwrap_or_else(PoisonError::into_inner).hotkey_vk();
        let mut msg = Msg { hwnd: 0, message: 0, wparam: 0, lparam: 0, time: 0, pt: Point { x: 0, y: 0 } };
        // Create this thread's message queue before anyone posts to it.
        unsafe { PeekMessageW(&mut msg, 0, WM_USER, WM_USER, PM_NOREMOVE) };
        // Kept registered while capturing: the hook sees the key first, and if
        // Windows ever drops the hook the message is still a way back.
        if unsafe { RegisterHotKey(0, HOTKEY_ID, MOD_NOREPEAT, vk) } == 0 {
            let _ = ready.send(Err("the hotkey is already registered by another program (change it with KFM_KEYMAP)".into()));
            return;
        }
        sh.tid.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        let _ = ready.send(Ok(()));
        let mut hook = 0isize;
        while unsafe { GetMessageW(&mut msg, 0, 0, 0) } > 0 {
            if msg.message == WM_HOTKEY && msg.wparam == HOTKEY_ID as usize {
                let down = down_keys();
                sh.with(|ws| ((), ws.on_hotkey_message(sh.now(), &down)));
            }
            sync_hook(sh, &mut hook);
        }
        if hook != 0 { unsafe { UnhookWindowsHookEx(hook) }; }
        unsafe { UnregisterHotKey(0, HOTKEY_ID) };
    }

    fn arg(args: &[String], name: &str) -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    }
    fn flag(args: &[String], name: &str) -> bool { args.iter().any(|a| a == name) }
    fn ranged(args: &[String], name: &str, range: std::ops::RangeInclusive<u64>) -> Result<Option<u64>, String> {
        arg(args, name).map(|v| v.parse().ok().filter(|n| range.contains(n)).ok_or(format!("{name} must be {}..{}", range.start(), range.end()))).transpose()
    }

    fn on_ctrl_c() {
        unsafe extern "system" fn handler(_: u32) -> i32 { STOP.store(true, Ordering::SeqCst); 1 }
        unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
    }

    fn cursor() -> Option<(i32, i32)> {
        let mut p = Point { x: 0, y: 0 };
        // Fails on the secure desktop (lock screen, UAC): no sample.
        (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
    }

    /// Monitors in virtual-screen pixels (the process is made DPI-aware first so
    /// these and GetCursorPos use the same units), and whether each is primary.
    fn monitors() -> Vec<(Rect, bool)> {
        unsafe extern "system" fn each(m: isize, _: isize, _: *mut RectI, data: isize) -> i32 {
            let z = || RectI { l: 0, t: 0, r: 0, b: 0 };
            let mut mi = MonitorInfo { size: std::mem::size_of::<MonitorInfo>() as u32, monitor: z(), work: z(), flags: 0 };
            if unsafe { GetMonitorInfoW(m, &mut mi) } != 0 {
                let out = unsafe { &mut *(data as *mut Vec<(Rect, bool)>) };
                let r = &mi.monitor;
                out.push((Rect { x: r.l.into(), y: r.t.into(), w: (r.r - r.l).into(), h: (r.b - r.t).into() }, mi.flags & 1 != 0));
            }
            1
        }
        let mut v: Vec<(Rect, bool)> = Vec::new();
        unsafe {
            SetProcessDPIAware();
            EnumDisplayMonitors(0, std::ptr::null(), each, &mut v as *mut _ as isize);
        }
        v
    }

    /// Flow rule C as kfm-mac runs it: this PC's cursor at the edge facing the
    /// receiver plus the receiver's cursor entering = the mouse went there.
    struct Flow { rects: Vec<Rect>, candidates: Vec<usize>, edge: Edge, tracker: PointerTracker, obs: Observer }

    impl Flow {
        fn from_args(args: &[String], rects: &[Rect]) -> Result<Option<Flow>, String> {
            let Some(edge) = arg(args, "--flow-edge") else {
                return if flag(args, "--flow-display") { Err("--flow-display needs --flow-edge".into()) } else { Ok(None) };
            };
            let edge: Edge = edge.parse()?;
            if rects.is_empty() { return Err("no display found".into()); }
            let candidates: Result<Vec<usize>, String> = match arg(args, "--flow-display").as_deref() {
                None if rects.len() == 1 => Ok(vec![0]),
                None => Err(format!("{} displays: pass --flow-display all or N[,M] (1-based, as listed above)", rects.len())),
                Some("all") => Ok((0..rects.len()).collect()),
                Some(list) => list.split(',').map(|n| n.trim().parse::<usize>().ok().filter(|&n| n >= 1 && n <= rects.len()).map(|n| n - 1)
                    .ok_or(format!("--flow-display: {n:?} is not 1..{}", rects.len()))).collect(),
            };
            let chosen = candidates.as_ref().map(Vec::as_slice).unwrap_or(&[]);
            say!("flow: {edge:?} edge of this PC faces the receiver; {} display(s):", rects.len());
            for (i, r) in rects.iter().enumerate() {
                say!("  display {}: {}x{}, {edge:?} edge facing outward: {}, candidate: {}", i + 1, r.w, r.h,
                    match outer_edge_share(rects, i, edge) { 0 => "no", 100 => "yes", _ => "partly" }, if chosen.contains(&i) { "yes" } else { "no" });
            }
            Ok(Some(Flow { rects: rects.to_vec(), candidates: candidates?, edge, tracker: PointerTracker::default(), obs: Observer::new(FlowConfig::default(), rects.len()) }))
        }

        fn on_pointer(&mut self, x: i32, y: i32, now: u64) -> Vec<Note> {
            let mut notes = vec![];
            if let Some(ev) = self.tracker.on_sample(&self.rects, &self.candidates, self.edge, x.into(), y.into()) {
                notes.extend(self.obs.on_pointer(ev, now, self.tracker.from_edge_pt()));
            }
            notes.extend(self.obs.tick(now));
            notes
        }
    }

    fn stamp(now: u64) -> String { format!("+{}.{:03}s", now / 1000, now % 1000) }

    struct Net { link: MacLink, sock: UdpSocket, peer: SocketAddr, sent_down: u64, sent_up: u64 }

    impl Net {
        fn send_key(&mut self, key: keyboard_follows_mouse::keys::WinKey, down: bool, repeat: bool, now: u64) {
            if down { self.sent_down += 1 } else { self.sent_up += 1 }
            if let Some(f) = self.link.key(key, down, repeat, now) { let _ = self.sock.send_to(&f, self.peer); }
        }
    }

    /// Prints state changes only (never a key) and sends key events.
    fn apply(e: Effect, net: &mut Net, active: bool, hotkey: &str, now: u64) {
        match e {
            Effect::Remote { key, down, repeat } => net.send_key(key, down, repeat, now),
            Effect::Capture { on: true, why } => if active {
                say!("[{}] CAPTURING ({why:?}): every keyboard on this PC goes to the receiver. Press {hotkey} to return.", stamp(now));
            } else { say!("[{}] [dry-run] would capture all keyboards ({why:?}); nothing is captured", stamp(now)) },
            Effect::Capture { on: false, why } => say!("[{}] back to local ({why:?}): keys stay on this PC", stamp(now)),
            Effect::Holding { on: true } => say!("[{}] link down: input is held (swallowed, not typed here)", stamp(now)),
            Effect::Holding { on: false } => say!("[{}] link back: forwarding again", stamp(now)),
            Effect::Refused { why } => say!("[{}] capture refused ({why:?}): the link to the receiver is not confirmed", stamp(now)),
        }
    }

    fn run(args: &[String]) -> Result<(), String> {
        start_printer();
        let active = flag(args, "--experimental-capture-all-keyboards");
        let peer: SocketAddr = arg(args, "--peer").ok_or("--peer IP:PORT is required")?.parse().map_err(|e| format!("--peer: {e}"))?;
        let bind = arg(args, "--bind").ok_or("--bind IP:PORT is required")?;
        let key = keyfile::load(Path::new(&arg(args, "--key-file").ok_or("--key-file is required")?))?;
        // keyfile::load checks owner-only mode on Unix only; the Windows ACL is not checked yet.
        say!("kfm-send: key file ACL is not checked on Windows; restrict it with icacls <file> /inheritance:r /grant:r \"%USERNAME%:R\"");
        let mut timing = Timing::default();
        if let Some(ms) = ranged(args, "--link-timeout-ms", 100..=2000)? { timing.link_timeout_ms = ms; }
        let mut cfg = Config::default();
        if let Some(ms) = ranged(args, "--link-loss-stop-ms", 500..=60_000)? { cfg.link_loss_stop_ms = ms; }
        if let Some(s) = ranged(args, "--idle-stop-secs", 10..=86_400)? { cfg.idle_stop_ms = s * 1000; }

        let keys = keyboard_follows_mouse::remap::key_config();
        let profile = WindowsProfile::identity().with_user(keys.remaps(TargetOs::Mac));
        let ws = WinSource::new(profile, keys.hotkey_router_usage(), cfg)?;
        let rects: Vec<Rect> = monitors().into_iter().map(|(r, _)| r).collect();
        let mut flow = Flow::from_args(args, &rects)?;

        let sock = UdpSocket::bind(&bind).map_err(|e| format!("bind {bind}: {e}"))?;
        sock.set_nonblocking(true).map_err(|e| e.to_string())?;
        let mut net = Net { link: MacLink::new(key, timing), sock, peer, sent_down: 0, sent_up: 0 };

        let (tx, rx) = mpsc::channel();
        if SHARED.set(Shared { ws: Mutex::new(ws), tx, tid: AtomicU32::new(0), start: Instant::now() }).is_err() { return Err("already running".into()); }
        let sh = SHARED.get().ok_or("state not set")?;
        on_ctrl_c();

        let hotkey = keys.hotkey.to_uppercase();
        let ui = if active {
            say!("kfm-send: EXPERIMENTAL capture ENABLED. While forwarding, ALL keyboards on this PC (a laptop's built-in one too) go to {peer}; at rest nothing is hooked.");
            say!("kfm-send: hotkey {hotkey} toggles (needs the link up). RECOVERY while forwarding: press {hotkey}, or move the mouse back, or close this console window with the mouse. Ctrl+C is captured too and works only when not forwarding. The automatic stops also return. Windows removes the hook when this program ends. The lock screen / UAC / Ctrl+Alt+Del always stay local.");
            let (rtx, rrx) = mpsc::channel();
            let h = std::thread::Builder::new().name("kfm-send-ui".into()).spawn(move || ui_thread(rtx)).map_err(|e| e.to_string())?;
            match rrx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err("the UI thread did not start".into()),
            }
            Some(h)
        } else {
            say!("kfm-send: dry run (no --experimental-capture-all-keyboards): no hook, no hotkey, nothing suppressed; link and Flow are only reported");
            None
        };
        say!("kfm-send: peer {peer}, bind {bind}; stops capturing after the link is down {} ms or {} s without a forwarded key", cfg.link_loss_stop_ms, cfg.idle_stop_ms / 1000);

        let mut mouse = LocalMouse::new(MOUSE_RETURN_PX);
        let hotkey_vk = sh.ws.lock().unwrap_or_else(PoisonError::into_inner).hotkey_vk();
        let (mut last_cursor_poll, mut error) = (0u64, None::<String>);
        let mut buf = [0u8; 512];
        while !STOP.load(Ordering::SeqCst) {
            if ui.as_ref().is_some_and(|h| h.is_finished()) { error = Some("the UI thread stopped".into()); break; }
            // Effects from the hook / hotkey; waking on one keeps the key latency low.
            match rx.recv_timeout(Duration::from_millis(2)) {
                Ok(e) => apply(e, &mut net, active, &hotkey, sh.now()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let now = sh.now();
            let mut changes = vec![];
            while let Ok((n, from)) = net.sock.recv_from(&mut buf) {
                if from == net.peer { changes.extend(net.link.on_datagram(&buf[..n], now)); }
            }
            let (hb, c) = net.link.tick(now);
            changes.extend(c);
            if let Some(f) = hb { let _ = net.sock.send_to(&f, net.peer); }
            let mut notes = vec![];
            for c in changes {
                say!("[{}] link {}", stamp(now), if c == LinkChange::Up { "up" } else { "down" });
                sh.with(|ws| ((), ws.on_link(c == LinkChange::Up, now)));
                if c == LinkChange::Down && let Some(f) = flow.as_mut() { notes.extend(f.obs.on_link_down(now)); }
            }
            let p = net.link.take_peer();
            if now.saturating_sub(last_cursor_poll) >= CURSOR_POLL_MS {
                last_cursor_poll = now;
                match (cursor(), flow.as_mut()) {
                    (Some((x, y)), Some(f)) => notes.extend(f.on_pointer(x, y, now)),
                    (Some((x, y)), None) => if mouse.moved_far(sh.wants_hook(), x, y) { sh.with(|ws| ((), ws.on_local_mouse_activity())); },
                    // No cursor: the secure desktop (lock screen, UAC, Ctrl+Alt+Del) has input and the hook
                    // does not see it. Give the keys back now instead of leaving Ctrl/Alt held on the receiver.
                    (None, _) => if sh.wants_hook() { sh.with(|ws| ((), ws.on_secure_desktop())); },
                }
                // The hotkey seen up clears the "just handled" guards (the hook may already be gone).
                if active && unsafe { GetAsyncKeyState(hotkey_vk as i32) } >= 0 { sh.with(|ws| { ws.on_hotkey_released(); ((), vec![]) }); }
            }
            if let Some(f) = flow.as_mut() {
                for (n, e) in [(p.entered, WinEdgeEvent::Entered), (p.departed, WinEdgeEvent::Departed), (p.reached, WinEdgeEvent::Reached)] {
                    for _ in 0..n.min(8) { notes.extend(f.obs.on_win_edge(e, now)); }
                }
                if p.moved { notes.extend(f.obs.on_win_activity(now)); }
                for n in notes {
                    let down = if active { down_keys() } else { vec![] };
                    match n {
                        Note::ToWindows { rule: Rule::C, .. } => {
                            say!("[{}] flow: the mouse went to the receiver", stamp(now));
                            sh.with(|ws| ((), ws.on_flow(Target::Windows, now, &down)));
                        }
                        Note::ToMac { rule: Rule::C, .. } => {
                            say!("[{}] flow: the mouse is back on this PC", stamp(now));
                            sh.with(|ws| ((), ws.on_flow(Target::Mac, now, &down)));
                        }
                        _ => {}
                    }
                }
            }
            sh.with(|ws| ((), ws.tick(now)));
            while let Ok(e) = rx.try_recv() { apply(e, &mut net, active, &hotkey, sh.now()); }
        }

        // Release everything held on the receiver, then let the UI thread remove the hook.
        sh.with(|ws| ((), ws.shutdown()));
        while let Ok(e) = rx.try_recv() { apply(e, &mut net, active, &hotkey, sh.now()); }
        if let Some(h) = ui {
            unsafe { PostThreadMessageW(sh.tid.load(Ordering::SeqCst), WM_QUIT, 0, 0) };
            let _ = h.join();
        }
        let (c, drops) = { let ws = sh.ws.lock().unwrap_or_else(PoisonError::into_inner); (ws.counters(), ws.held_drops()) };
        say!("stopped: captures {}, auto-stops {}, key presses forwarded {}, let through while capturing {}, no equivalent (swallowed) {}, swallowed while the link was down {drops}; sent {} down / {} up",
            c.captures, c.auto_stops, c.forwarded, c.passes, c.unmapped, net.sent_down, net.sent_up);
        flush();
        error.map_or(Ok(()), Err)
    }

    pub fn main() -> ExitCode {
        if let Err(e) = keyboard_follows_mouse::remap::key_config_from_env() { eprintln!("kfm-send: {e}"); return ExitCode::FAILURE; }
        let args: Vec<String> = std::env::args().skip(1).collect();
        match run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => { flush(); eprintln!("kfm-send: {e}"); ExitCode::FAILURE }
        }
    }
}
