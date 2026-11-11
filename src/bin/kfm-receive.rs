//! Receiver (Windows or macOS). Dry-run by default (counts what it would
//! inject); real injection only with `--enable-inject`: SendInput on Windows,
//! CGEventPost on macOS (needs the Accessibility permission for the app that
//! started this process: the terminal when run by hand, the binary under launchd). Accepts datagrams only
//! from the pinned peer address that authenticate under its current challenge.
//! Key contents are never logged. It also polls this PC's cursor position
//! (GetCursorPos, no hook) and reports only "it moved" in its Acks, for the
//! Mac's Flow follower, plus events at this PC's outer left edge (the side
//! facing the Mac): entered / departed / reached. Coordinates are never sent or
//! logged.
//!
//!   kfm-receive --bind IP:PORT --peer IP:PORT --key-file PATH [--enable-inject]
//!           [--seconds N] [--stop-file PATH] [--mx4-id TEXT|off]
//!           [--flow-edge left|right|top|bottom] [--flow-display all|N[,M]]
//! `--flow-edge` (default left): this computer's outer edge that faces the
//! keyboard's host in your Flow layout; `--flow-display` (default all): which
//! of this computer's displays have that edge (1-based, as listed at start).
//! `--list-displays`: print this computer's displays (index, origin, size) and exit; nothing else runs.
//! `--stop-file`: when that file appears (checked every 200 ms), stop like
//! Ctrl+C: every held key is released before exiting.
//! `--mx4-id` (default `vid&02046d_pid&b042`): every 200 ms, is a raw-input
//! mouse whose device path contains TEXT (case-insensitive) in the device list
//! (GetRawInputDeviceList, no hook)? The Ack carries present / arrivals /
//! departures for the Mac's rule E. Device paths are never sent or logged.

use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use keyboard_follows_mouse::flow::{outer_edge_share, Edge, EntryTracker, Rect};
#[cfg(target_os = "macos")]
use keyboard_follows_mouse::macinject::MacKeyboard;
use keyboard_follows_mouse::keyfile;
use keyboard_follows_mouse::keys::WinKey;
use keyboard_follows_mouse::link::Injector;
use keyboard_follows_mouse::transport::{Timing, WinLink};

#[derive(Default)]
struct DryRun { downs: u64, ups: u64 }
impl Injector for DryRun {
    fn inject(&mut self, _: WinKey, down: bool) { if down { self.downs += 1 } else { self.ups += 1 } }
}

#[cfg(windows)]
mod sendinput {
    use keyboard_follows_mouse::keys::WinKey;
    use keyboard_follows_mouse::link::Injector;

    #[repr(C)]
    struct KeybdInput { vk: u16, scan: u16, flags: u32, time: u32, extra: usize }
    /// INPUT with the union sized for MOUSEINPUT (the largest member).
    #[repr(C)]
    struct Input { kind: u32, ki: KeybdInput, _pad: [u8; 8] }
    const _: () = assert!(std::mem::size_of::<Input>() == 40);

    const INPUT_KEYBOARD: u32 = 1;
    const KEYEVENTF_EXTENDEDKEY: u32 = 0x1;
    const KEYEVENTF_KEYUP: u32 = 0x2;
    const KEYEVENTF_SCANCODE: u32 = 0x8;
    /// Marks our events (dwExtraInfo) so they can be told apart later.
    const MARK: usize = 0x5237_3500;

    #[link(name = "user32")]
    unsafe extern "system" { fn SendInput(n: u32, inputs: *const Input, size: i32) -> u32; }

    #[derive(Default)]
    pub struct Real { pub downs: u64, pub ups: u64, pub failures: u64 }
    impl Injector for Real {
        fn inject(&mut self, key: WinKey, down: bool) {
            if down { self.downs += 1 } else { self.ups += 1 }
            let up = if down { 0 } else { KEYEVENTF_KEYUP };
            let ki = match key {
                WinKey::Scan { code, extended } => KeybdInput { vk: 0, scan: code.into(), flags: KEYEVENTF_SCANCODE | up | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }, time: 0, extra: MARK },
                WinKey::Vk(vk) => KeybdInput { vk: vk.into(), scan: 0, flags: up, time: 0, extra: MARK },
            };
            let input = Input { kind: INPUT_KEYBOARD, ki, _pad: [0; 8] };
            // 0 = blocked (UIPI / secure desktop): counted, never retried blindly.
            if unsafe { SendInput(1, &input, std::mem::size_of::<Input>() as i32) } != 1 { self.failures += 1; }
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    #[repr(C)] #[derive(Clone, Copy)] pub struct CgPoint { pub x: f64, pub y: f64 }
    #[repr(C)] #[derive(Clone, Copy)] pub struct CgSize { pub w: f64, pub h: f64 }
    #[repr(C)] #[derive(Clone, Copy)] pub struct CgRect { pub origin: CgPoint, pub size: CgSize }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub fn CGEventCreate(source: *const c_void) -> *mut c_void;
        pub fn CGEventGetLocation(event: *const c_void) -> CgPoint;
        pub fn CGGetActiveDisplayList(max: u32, ids: *mut u32, count: *mut u32) -> i32;
        pub fn CGDisplayBounds(id: u32) -> CgRect;
        pub fn CGMainDisplayID() -> u32;
        pub fn CGEventCreateKeyboardEvent(source: *const c_void, keycode: u16, down: bool) -> *mut c_void;
        pub fn CGEventSetType(event: *mut c_void, kind: u32);
        pub fn CGEventSetFlags(event: *mut c_void, flags: u64);
        pub fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
        pub fn CGEventPost(tap: u32, event: *mut c_void);
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" { pub fn CFRelease(r: *const c_void); }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" { pub fn AXIsProcessTrusted() -> bool; }

    pub const K_CG_HID_EVENT_TAP: u32 = 0;
    pub const K_CG_EVENT_FLAGS_CHANGED: u32 = 12;
    /// kCGEventSourceUserData: marks our events so a capture layer can skip them.
    pub const K_CG_EVENT_SOURCE_USER_DATA: u32 = 42;
    pub const MARK: i64 = 0x4B46_4D00;

    #[derive(Default)]
    pub struct Real { pub kb: super::MacKeyboard, pub downs: u64, pub ups: u64, pub unmapped: u64, pub failures: u64 }
    impl keyboard_follows_mouse::link::Injector for Real {
        fn inject(&mut self, key: keyboard_follows_mouse::keys::WinKey, down: bool) {
            if down { self.downs += 1 } else { self.ups += 1 }
            let Some(e) = self.kb.event(key, down) else { self.unmapped += 1; return };
            unsafe {
                let ev = CGEventCreateKeyboardEvent(std::ptr::null(), e.keycode, e.down);
                if ev.is_null() { self.failures += 1; return; }
                if e.modifier { CGEventSetType(ev, K_CG_EVENT_FLAGS_CHANGED); }
                CGEventSetFlags(ev, e.flags);
                CGEventSetIntegerValueField(ev, K_CG_EVENT_SOURCE_USER_DATA, MARK);
                CGEventPost(K_CG_HID_EVENT_TAP, ev);
                CFRelease(ev);
            }
        }
    }
}

/// Set by Ctrl+C / console close so `serve` can release held keys before exit.
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
fn on_ctrl_c() {
    unsafe extern "system" fn handler(_: u32) -> i32 { STOP.store(true, std::sync::atomic::Ordering::SeqCst); 1 }
    #[link(name = "kernel32")]
    unsafe extern "system" { fn SetConsoleCtrlHandler(h: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32; }
    unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
}
#[cfg(unix)]
fn on_ctrl_c() {
    extern "C" fn handler(_: libc::c_int) { STOP.store(true, std::sync::atomic::Ordering::SeqCst); }
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
    }
}
#[cfg(not(any(windows, unix)))]
fn on_ctrl_c() {}

#[cfg(windows)]
fn cursor() -> Option<(i32, i32)> {
    #[repr(C)]
    struct Point { x: i32, y: i32 }
    #[link(name = "user32")]
    unsafe extern "system" { fn GetCursorPos(p: *mut Point) -> i32; }
    let mut p = Point { x: 0, y: 0 };
    // Fails on the secure desktop (lock screen, UAC): no sample, no movement.
    (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
}
#[cfg(target_os = "macos")]
fn cursor() -> Option<(i32, i32)> {
    // Global display coordinates in points (the same space as CGDisplayBounds).
    unsafe {
        let ev = mac::CGEventCreate(std::ptr::null());
        if ev.is_null() { return None; }
        let p = mac::CGEventGetLocation(ev);
        mac::CFRelease(ev);
        Some((p.x.floor() as i32, p.y.floor() as i32)) // floor: a fractional point on the last column stays on the display
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
fn cursor() -> Option<(i32, i32)> { None }

/// Monitors in virtual-screen pixels, and whether each is the primary one. The
/// process is made DPI-aware first so these and GetCursorPos use the same units.
#[cfg(windows)]
fn monitors() -> Vec<(Rect, bool)> {
    #[repr(C)]
    struct RectI { l: i32, t: i32, r: i32, b: i32 }
    #[repr(C)]
    struct MonitorInfo { size: u32, monitor: RectI, work: RectI, flags: u32 }
    type EnumProc = unsafe extern "system" fn(isize, isize, *mut RectI, isize) -> i32;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetProcessDPIAware() -> i32;
        fn EnumDisplayMonitors(hdc: isize, clip: *const RectI, cb: EnumProc, data: isize) -> i32;
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    }
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
#[cfg(target_os = "macos")]
fn monitors() -> Vec<(Rect, bool)> {
    let mut ids = [0u32; 32];
    let mut n = 0u32;
    if unsafe { mac::CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) } != 0 { return vec![]; }
    let main = unsafe { mac::CGMainDisplayID() };
    ids[..n as usize].iter().map(|id| {
        let b = unsafe { mac::CGDisplayBounds(*id) };
        (Rect { x: b.origin.x, y: b.origin.y, w: b.size.w, h: b.size.h }, *id == main)
    }).collect()
}
#[cfg(not(any(windows, target_os = "macos")))]
fn monitors() -> Vec<(Rect, bool)> { vec![] }

/// Is a raw-input mouse whose device path contains `id` (lowercase) in the
/// list? `None` when the list cannot be read (no sample taken).
#[cfg(windows)]
fn mx4_present(id: &str) -> Option<bool> {
    #[repr(C)]
    struct RawInputDeviceList { device: isize, kind: u32 }
    const RIM_TYPEMOUSE: u32 = 0;
    const RIDI_DEVICENAME: u32 = 0x2000_0007;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetRawInputDeviceList(list: *mut RawInputDeviceList, n: *mut u32, size: u32) -> u32;
        fn GetRawInputDeviceInfoW(device: isize, cmd: u32, data: *mut u16, size: *mut u32) -> u32;
    }
    let size = std::mem::size_of::<RawInputDeviceList>() as u32;
    let mut n = 0u32;
    if unsafe { GetRawInputDeviceList(std::ptr::null_mut(), &mut n, size) } == u32::MAX { return None; }
    // A device may arrive between the two calls: ask again once with room to spare.
    let mut list: Vec<RawInputDeviceList> = (0..n + 4).map(|_| RawInputDeviceList { device: 0, kind: 0 }).collect();
    let mut cap = list.len() as u32;
    let got = unsafe { GetRawInputDeviceList(list.as_mut_ptr(), &mut cap, size) };
    if got == u32::MAX { return None; }
    list.truncate(got as usize);
    Some(list.iter().filter(|d| d.kind == RIM_TYPEMOUSE).any(|d| {
        let mut len = 0u32;
        unsafe { GetRawInputDeviceInfoW(d.device, RIDI_DEVICENAME, std::ptr::null_mut(), &mut len) };
        let mut name = vec![0u16; len as usize + 1];
        let n = unsafe { GetRawInputDeviceInfoW(d.device, RIDI_DEVICENAME, name.as_mut_ptr(), &mut len) };
        n != u32::MAX && n > 0 && String::from_utf16_lossy(&name[..n as usize]).to_lowercase().contains(id)
    }))
}
/// Not Windows: no device list. Tests stand in for the MX4 with a file
/// (KFM_TEST_MX4_FILE: present while it exists).
#[cfg(not(windows))]
fn mx4_present(_: &str) -> Option<bool> {
    std::env::var_os("KFM_TEST_MX4_FILE").map(|f| Path::new(&f).exists())
}

const CURSOR_POLL_MS: u64 = 20;
const MX4_POLL_MS: u64 = 200;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[allow(clippy::too_many_arguments)] // one call site per injector; a struct would only rename them
fn serve(sock: &UdpSocket, peer: SocketAddr, link: &mut WinLink, inj: &mut impl Injector, stop_after: Option<Duration>, stop_file: Option<&Path>, mx4_id: Option<&str>, mut entry: EntryTracker) {
    let start = Instant::now();
    let (mut last_stop_check, mut last_mx4) = (0u64, 0u64);
    let mut buf = [0u8; 512];
    let (mut ok, mut foreign) = (0u64, 0u64);
    let mut last_report = 0u64;
    let mons: Vec<Rect> = monitors().into_iter().map(|(r, _)| r).collect();
    let (mut last_cursor, mut last_poll) = (cursor(), 0u64);
    loop {
        let now = start.elapsed().as_millis() as u64;
        if STOP.load(std::sync::atomic::Ordering::SeqCst) || stop_after.is_some_and(|d| start.elapsed() >= d) { break; }
        if let Some(f) = stop_file && now - last_stop_check >= 200 {
            last_stop_check = now;
            if f.exists() { println!("kfm-receive: stop file found; releasing keys and stopping"); break; }
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from == peer => if let Ok(ack) = link.on_datagram(&buf[..n], now, inj) { ok += 1; let _ = sock.send_to(&ack, peer); },
            Ok(_) => foreign += 1, // not the pinned peer: ignored without reply
            Err(_) => {}
        }
        if let Some(id) = mx4_id && now - last_mx4 >= MX4_POLL_MS {
            last_mx4 = now;
            if let Some(present) = mx4_present(id) && link.mx4_sample(present) {
                println!("[mx4 +{}.{:03}s] MX4 {}", now / 1000, now % 1000, if present { "arrived" } else { "left" });
                let _ = sock.send_to(&link.ack_now(now), peer);
            }
        }
        if now - last_poll >= CURSOR_POLL_MS {
            last_poll = now;
            let c = cursor();
            let mut report = false;
            if c.is_some() && last_cursor.is_some() && c != last_cursor { link.mouse_moved(now); report = true; }
            if let Some((x, y)) = c {
                last_cursor = c;
                if let Some(e) = entry.on_sample(&mons, x.into(), y.into()) { link.edge_event(e); report = true; }
            }
            // Tell the Mac at once rather than at the next heartbeat.
            if report { let _ = sock.send_to(&link.ack_now(now), peer); }
        }
        if let Some(hello) = link.tick(now, inj) { let _ = sock.send_to(&hello, peer); }
        if now - last_report >= 10_000 {
            last_report = now;
            println!("frames ok {ok}, rejected {}, foreign {foreign}, keys held now {}, mouse moves {}, mx4 {}", link.rejected, link.held(), link.mouse_moves(), if link.mx4_present() { "present" } else { "absent" });
        }
    }
    link.release_all(inj);
    let [entered, departed, reached] = link.edge_counts();
    let [arrived, left] = link.mx4_counts();
    println!("final: frames ok {ok}, rejected {}, foreign {foreign}, mouse moves reported {}, facing edge: entered {entered}, departed {departed}, reached {reached}, mx4 arrived {arrived}, left {left}", link.rejected, link.mouse_moves());
}

fn run(args: &[String]) -> Result<(), String> {
    // For `kfm-config layout displays`: one line per display (1-based, as --flow-display counts), then exit.
    if args.iter().any(|a| a == "--list-displays") {
        for (i, (r, primary)) in monitors().iter().enumerate() { println!("display {} x={} y={} w={} h={} primary={primary}", i + 1, r.x, r.y, r.w, r.h); }
        return Ok(());
    }
    let bind = arg(args, "--bind").ok_or("--bind IP:PORT is required")?;
    let peer: SocketAddr = arg(args, "--peer").ok_or("--peer IP:PORT is required")?.parse().map_err(|e| format!("--peer: {e}"))?;
    let key = keyfile::load(Path::new(&arg(args, "--key-file").ok_or("--key-file is required")?))?;
    // keyfile::load checks owner-only mode on Unix only; the Windows ACL is not checked yet.
    #[cfg(not(unix))]
    println!("kfm-receive: key file ACL is not checked on Windows; restrict it with icacls <file> /inheritance:r /grant:r \"%USERNAME%:R\"");
    let sock = UdpSocket::bind(&bind).map_err(|e| format!("bind {bind}: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let mut link = WinLink::new(key, Timing::default());
    on_ctrl_c();
    let mons = monitors();
    let rects: Vec<Rect> = mons.iter().map(|(r, _)| *r).collect();
    let edge: Edge = arg(args, "--flow-edge").unwrap_or_else(|| "left".into()).parse().map_err(|e| format!("--flow-edge: {e}"))?;
    let displays: Option<Vec<usize>> = match arg(args, "--flow-display").as_deref() {
        None | Some("all") => None,
        Some(list) => Some(list.split(',').map(|n| n.trim().parse::<usize>().ok().filter(|n| *n >= 1 && *n <= mons.len().max(1)).map(|n| n - 1)
            .ok_or(if mons.is_empty() { "--flow-display: no displays found on this computer".to_string() } else { format!("--flow-display: {n:?} is not 1..{}", mons.len()) })).collect::<Result<_, _>>()?),
    };
    println!("kfm-receive: {} monitor(s); watching the {edge:?} edge (the side facing the keyboard's host) of {}:", mons.len(),
        displays.as_ref().map_or("all of them".to_string(), |d| d.iter().map(|i| format!("monitor {}", i + 1)).collect::<Vec<_>>().join(", ")));
    for (i, (r, primary)) in mons.iter().enumerate() {
        let outer = outer_edge_share(&rects, i, edge);
        println!("  monitor {}: {}x{}{}, {edge:?} edge facing outward: {}", i + 1, r.w, r.h, if *primary { " (primary)" } else { "" },
            match outer { 0 => "no", 100 => "yes", _ => "partly" });
    }
    let entry = EntryTracker::facing(edge, displays);
    let stop_after = arg(args, "--seconds").map(|s| Duration::from_secs(s.parse().unwrap_or(10)));
    let stop_file = arg(args, "--stop-file").map(std::path::PathBuf::from);
    if stop_file.as_deref().is_some_and(Path::exists) { return Err("--stop-file already exists; remove it to start".into()); }
    let mx4_id = match arg(args, "--mx4-id") {
        Some(v) if v == "off" => None,
        Some(v) if v.trim().is_empty() => return Err("--mx4-id needs TEXT (part of the device path) or off".into()),
        Some(v) => Some(v.to_lowercase()),
        None if cfg!(windows) => Some("vid&02046d_pid&b042".to_string()),
        // macOS has no device-list watch yet: Easy-Switch onto this receiver is not seen.
        None => None,
    };
    match &mx4_id {
        Some(id) => println!("kfm-receive: MX4 watch: raw-input mouse whose device path contains {id:?}, every {MX4_POLL_MS} ms (now {})",
            match mx4_present(id) { Some(true) => "present", Some(false) => "absent", None => "device list unavailable" }),
        None => println!("kfm-receive: MX4 watch off{}", if cfg!(windows) { "" } else { " (not available on this OS: Easy-Switch onto this computer is not detected)" }),
    }
    if args.iter().any(|a| a == "--enable-inject") {
        #[cfg(windows)]
        {
            println!("kfm-receive: INJECTING via SendInput; peer {peer}. Stop with Ctrl+C (keys are released on exit; watchdog {} ms).", Timing::default().watchdog_ms);
            let mut inj = sendinput::Real::default();
            serve(&sock, peer, &mut link, &mut inj, stop_after, stop_file.as_deref(), mx4_id.as_deref(), entry);
            println!("injected {} down / {} up (attempted); SendInput failures: {}", inj.downs, inj.ups, inj.failures);
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            // Without the permission CGEventPost drops events silently: refuse instead.
            if !unsafe { mac::AXIsProcessTrusted() } {
                return Err("--enable-inject on macOS needs the Accessibility permission (System Settings > Privacy & Security > Accessibility). macOS checks the app that started this process: the terminal when run by hand, this binary itself when started by launchd. Nothing was injected".into());
            }
            println!("kfm-receive: INJECTING via CGEventPost; peer {peer}. Stop with Ctrl+C (keys are released on exit; watchdog {} ms).", Timing::default().watchdog_ms);
            let mut inj = mac::Real::default();
            serve(&sock, peer, &mut link, &mut inj, stop_after, stop_file.as_deref(), mx4_id.as_deref(), entry);
            println!("injected {} down / {} up (attempted); no macOS key: {}; post failures: {}", inj.downs, inj.ups, inj.unmapped, inj.failures);
            return Ok(());
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        return Err("--enable-inject is only available on Windows and macOS".into());
    }
    println!("kfm-receive: dry-run (nothing injected); listening {bind}, peer {peer}");
    let mut inj = DryRun::default();
    serve(&sock, peer, &mut link, &mut inj, stop_after, stop_file.as_deref(), mx4_id.as_deref(), entry);
    println!("would have injected {} down / {} up", inj.downs, inj.ups);
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("kfm-receive: {e}"); ExitCode::FAILURE }
    }
}
