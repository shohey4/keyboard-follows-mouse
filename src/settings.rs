//! The settings behind `kfm-config`: one `KEY=VALUE` file (`local/config.sh`, the
//! format of `examples/config.example.sh`, sourceable by sh) and one keymap JSON
//! (`local/keymap.json`, parsed by `remap::parse`). This module is pure logic and
//! plain file handling: no OS input, no network, no process spawning (the binary
//! gathers OS facts and passes them in). Every edit is validated as a whole, the
//! previous files are backed up, and files are replaced atomically.
//!
//! Two levels of validation, so settings can be built up one at a time:
//! `errors` (a value is malformed: nothing is written) and `missing` (a value the
//! role needs is empty: allowed while editing, reported by `validate`/`apply`).
//! Values are quoted for sh when written; a value that cannot be quoted safely
//! (a quote character or a control character) is refused. Key file contents are
//! never read here; only paths and file metadata are.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::flow::{outer_edge_share, Edge, Rect};
use crate::karabiner::{keyboard_from, name_of, usage_of};
use crate::keymap::Action;
use crate::remap::{self, KeyConfig, TargetOs, EXTRA_HOTKEYS};

pub const KEYS: &[&str] = &[
    "KFM_ROLE", "KFM_KEY", "KFM_PEER", "KFM_BIND", "KFM_FLOW_EDGE", "KFM_FLOW_DISPLAY", "KFM_LINK_TIMEOUT",
    "KFM_TARGET_OS", "KFM_KEYMAP", "KFM_KEYBOARD_VENDOR_ID", "KFM_KEYBOARD_PRODUCT_ID",
];
/// The keys `scripts/resident/install.sh` writes to the installed config.sh (the rest stay in `local/`).
pub const INSTALLED_KEYS: &[&str] = &[
    "KFM_PEER", "KFM_BIND", "KFM_FLOW_EDGE", "KFM_FLOW_DISPLAY", "KFM_LINK_TIMEOUT",
    "KFM_KEYBOARD_VENDOR_ID", "KFM_KEYBOARD_PRODUCT_ID", "KFM_TARGET_OS",
];
const DOCS: &[(&str, &str)] = &[
    ("KFM_ROLE", "mac-source | receiver | windows-source (experimental): what THIS computer does"),
    ("KFM_KEY", "pre-shared key file (made by `kfm-config keygen --out PATH`, copied to the other computer by you); never printed"),
    ("KFM_PEER", "the OTHER computer's IP:PORT (receiver: the source; source: the receiver)"),
    ("KFM_BIND", "THIS computer's LAN IP:PORT"),
    ("KFM_FLOW_EDGE", "this computer's screen edge that faces the other computer: left | right | top | bottom"),
    ("KFM_FLOW_DISPLAY", "which display(s) of this computer have that edge: all, or N[,M] (1-based)"),
    ("KFM_LINK_TIMEOUT", "ms without a verified Ack before the link counts as down, 100..2000 (empty = 300); mac-source / windows-source"),
    ("KFM_TARGET_OS", "the receiver's OS: windows | mac; mac-source only"),
    ("KFM_KEYMAP", "keymap JSON (hotkey, remaps); the working copy is keymap.json next to this file"),
    ("KFM_KEYBOARD_VENDOR_ID", "only for a keyboard other than the tested one, as Karabiner-Elements shows it; mac-source only"),
    ("KFM_KEYBOARD_PRODUCT_ID", "see KFM_KEYBOARD_VENDOR_ID (set both or neither)"),
];

// ---- roles ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role { MacSource, Receiver, WindowsSource }

impl Role {
    pub fn parse(s: &str) -> Result<Role, String> {
        match s { "mac-source" => Ok(Role::MacSource), "receiver" => Ok(Role::Receiver), "windows-source" => Ok(Role::WindowsSource),
                  _ => Err(format!("KFM_ROLE must be mac-source, receiver or windows-source, got {s:?}")) }
    }
    pub fn as_str(self) -> &'static str { match self { Role::MacSource => "mac-source", Role::Receiver => "receiver", Role::WindowsSource => "windows-source" } }
    /// Does this role read `key`? (Others are kept in the file but ignored.)
    pub fn uses(self, key: &str) -> bool {
        match self {
            Role::MacSource => true,
            Role::Receiver => matches!(key, "KFM_ROLE" | "KFM_KEY" | "KFM_PEER" | "KFM_BIND" | "KFM_FLOW_EDGE" | "KFM_FLOW_DISPLAY"),
            Role::WindowsSource => matches!(key, "KFM_ROLE" | "KFM_KEY" | "KFM_PEER" | "KFM_BIND" | "KFM_FLOW_EDGE" | "KFM_FLOW_DISPLAY" | "KFM_LINK_TIMEOUT" | "KFM_KEYMAP"),
        }
    }
}

// ---- config.sh ----

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings(BTreeMap<String, String>);

impl Settings {
    pub fn get(&self, key: &str) -> &str { self.0.get(key).map_or("", String::as_str) }
    pub fn set(&mut self, key: &str, value: &str) { self.0.insert(key.to_string(), value.to_string()); }
    pub fn role(&self) -> Option<Role> { Role::parse(self.get("KFM_ROLE")).ok() }

    /// A fresh file for `role`; `dir` is where the keymap and the key file live.
    pub fn template(role: Role, dir: &Path) -> Settings {
        let mut s = Settings::default();
        s.set("KFM_ROLE", role.as_str());
        s.set("KFM_KEY", &dir.join("kfm-psk.key").to_string_lossy());
        s.set("KFM_KEYMAP", &dir.join("keymap.json").to_string_lossy());
        s.set("KFM_FLOW_EDGE", match role { Role::MacSource => "right", Role::Receiver => "left", Role::WindowsSource => "" });
        if role != Role::WindowsSource { s.set("KFM_FLOW_DISPLAY", "all"); }
        if role == Role::MacSource { s.set("KFM_TARGET_OS", "windows"); }
        s
    }

    pub fn parse(text: &str) -> Result<Settings, String> { Settings::parse_with_home(text, home_dir().as_deref()) }

    /// Accepts `KEY=v`, `KEY='v'`, `KEY="v"` (no expansions), a leading `~/`, blank and `#` lines and a
    /// trailing ` # comment`: the forms `examples/config.example.sh` and install.sh produce. Anything
    /// else (unknown keys, `export`, `$(...)`) is an error, like the rest of the project.
    pub fn parse_with_home(text: &str, home: Option<&str>) -> Result<Settings, String> {
        let mut s = Settings::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let at = |e: String| format!("line {}: {e}", n + 1);
            let (key, rest) = line.split_once('=').ok_or_else(|| at("expected KEY=VALUE".into()))?;
            if !KEYS.contains(&key) { return Err(at(format!("unknown key {key:?} (known: {})", KEYS.join(", ")))); }
            if s.0.contains_key(key) { return Err(at(format!("{key} is set twice"))); }
            let value = parse_value(rest, home).map_err(at)?;
            s.0.insert(key.to_string(), value);
        }
        Ok(s)
    }

    /// The file text: every known key, in order, each with its one-line meaning. Comments the user added by
    /// hand are not kept when a file is rewritten.
    pub fn render(&self) -> Result<String, String> {
        let mut out = String::from("# keyboard-follows-mouse settings, written by kfm-config (edit with `kfm-config set KEY VALUE`).\n# Sourceable by sh: `set -a; . local/config.sh; set +a`. See examples/config.example.sh.\n");
        for key in KEYS {
            check_quotable(key, self.get(key))?;
            let doc = DOCS.iter().find(|(k, _)| k == key).map_or("", |(_, d)| *d);
            out.push_str(&format!("\n# {doc}\n{key}={}\n", sh_value(self.get(key))));
        }
        Ok(out)
    }
}

pub fn home_dir() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty()).or_else(|| std::env::var("USERPROFILE").ok().filter(|h| !h.is_empty()))
}

fn parse_value(rest: &str, home: Option<&str>) -> Result<String, String> {
    let tail_ok = |t: &str| t.trim().is_empty() || (t.starts_with(char::is_whitespace) && t.trim_start().starts_with('#'));
    if let Some(r) = rest.strip_prefix('\'') {
        let end = r.find('\'').ok_or("unterminated single quote")?;
        if !tail_ok(&r[end + 1..]) { return Err("unexpected text after the closing quote (a quote inside a value is not supported)".into()); }
        return Ok(r[..end].to_string());
    }
    if let Some(r) = rest.strip_prefix('"') {
        let end = r.find('"').ok_or("unterminated double quote")?;
        if r[..end].contains(['$', '`', '\\']) { return Err("$, backtick and backslash are not supported inside double quotes; use single quotes".into()); }
        if !tail_ok(&r[end + 1..]) { return Err("unexpected text after the closing quote".into()); }
        return Ok(r[..end].to_string());
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (tok, tail) = rest.split_at(end);
    if !tail_ok(tail) { return Err("unexpected text after the value (quote values that contain spaces)".into()); }
    if let Some(path) = tok.strip_prefix("~/") {
        let h = home.ok_or("`~` used but HOME is not set")?;
        return Ok(format!("{}/{path}", h.trim_end_matches(['/', '\\'])));
    }
    if tok.chars().any(|c| !is_bare(c)) { return Err(format!("{tok:?}: quote this value with single quotes")); }
    Ok(tok.to_string())
}

fn is_bare(c: char) -> bool { c.is_ascii_alphanumeric() || "_./:,+-@%=".contains(c) }

/// Why a value cannot be written to a sh-sourceable file, if it cannot.
pub fn check_quotable(key: &str, v: &str) -> Result<(), String> {
    if v.chars().any(char::is_control) { return Err(format!("{key}: control characters (newline, tab, ...) are not allowed")); }
    if v.contains('\'') { return Err(format!("{key}: a single quote (') in a value cannot be written safely for sh; rename the path or pick another value")); }
    if v.len() > 1024 { return Err(format!("{key}: value is longer than 1024 bytes")); }
    Ok(())
}

/// Bare when every character is shell-safe, else single-quoted (callers checked `check_quotable`).
pub fn sh_value(v: &str) -> String {
    if v.chars().all(is_bare) { v.to_string() } else { format!("'{v}'") }
}

// ---- quoting for command lines ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell { Sh, PowerShell }

impl Shell {
    pub fn native() -> Shell { if cfg!(windows) { Shell::PowerShell } else { Shell::Sh } }
    pub fn quote(self, a: &str) -> String {
        if !a.is_empty() && a.chars().all(is_bare) { return a.to_string(); }
        match self {
            Shell::Sh => format!("'{}'", a.replace('\'', "'\\''")),
            Shell::PowerShell => format!("'{}'", a.replace('\'', "''")),
        }
    }
    pub fn join(self, args: &[String]) -> String { args.iter().map(|a| self.quote(a)).collect::<Vec<_>>().join(" ") }
    /// A command line: `prog args...` (PowerShell needs the call operator for a quoted program path).
    pub fn command(self, prog: &str, args: &[String]) -> String {
        let p = self.quote(prog);
        let head = if self == Shell::PowerShell && p.starts_with('\'') { format!("& {p}") } else { p };
        if args.is_empty() { head } else { format!("{head} {}", self.join(args)) }
    }
    pub fn env_prefix(self, k: &str, v: &str) -> String {
        match self { Shell::Sh => format!("{k}={} ", self.quote(v)), Shell::PowerShell => format!("$env:{k} = {}; ", self.quote(v)) }
    }
}

// ---- per-value checks ----

fn is_absolute_like(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with("\\\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\'))
}

/// `./local/x` or `local/x` against `cwd`, without `.` components (no filesystem access).
pub fn resolve_path(v: &str, cwd: &Path) -> String {
    let mut out = cwd.to_path_buf();
    for part in Path::new(v).components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => { out.pop(); }
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

fn usb_id(name: &str, v: &str) -> Result<(), String> {
    // Same rule as the binaries (karabiner::keyboard_from): 1..=0xFFFF, decimal or 0x-hex.
    let r = if name.contains("VENDOR") { keyboard_from(Some(v), Some("0x1")) } else { keyboard_from(Some("0x1"), Some(v)) };
    r.map(|_| ()).map_err(|_| format!("{name} must be a USB id as Karabiner-Elements shows it, e.g. 0x1234 (1..=0xFFFF), got {v:?}"))
}

/// Is `value` well-formed for `key`? An empty value is always well-formed (it means unset; `missing` catches the rest).
pub fn check_value(key: &str, value: &str) -> Result<(), String> {
    if !KEYS.contains(&key) { return Err(format!("unknown key {key:?} (known: {})", KEYS.join(", "))); }
    check_quotable(key, value)?;
    if value.is_empty() { return Ok(()); }
    match key {
        "KFM_ROLE" => Role::parse(value).map(|_| ()),
        "KFM_KEY" | "KFM_KEYMAP" => if is_absolute_like(value) { Ok(()) } else { Err(format!("{key} must be an absolute path (or start with ~/), got {value:?}")) },
        "KFM_PEER" | "KFM_BIND" => match value.parse::<SocketAddr>() {
            Ok(a) if a.port() != 0 => Ok(()),
            _ => Err(format!("{key} must be IP:PORT, e.g. 192.0.2.10:47501, got {value:?}")),
        },
        "KFM_FLOW_EDGE" => value.parse::<Edge>().map(|_| ()).map_err(|_| format!("KFM_FLOW_EDGE must be left, right, top or bottom, got {value:?}")),
        "KFM_FLOW_DISPLAY" => display_selection(value, None).map(|_| ()),
        "KFM_LINK_TIMEOUT" => value.parse::<u32>().ok().filter(|ms| (100..=2000).contains(ms)).map(|_| ()).ok_or_else(|| format!("KFM_LINK_TIMEOUT must be 100..2000 (ms), got {value:?}")),
        "KFM_TARGET_OS" => TargetOs::parse(value).map(|_| ()).map_err(|_| format!("KFM_TARGET_OS must be windows or mac, got {value:?}")),
        "KFM_KEYBOARD_VENDOR_ID" | "KFM_KEYBOARD_PRODUCT_ID" => usb_id(key, value),
        _ => Ok(()),
    }
}

/// `all` -> None, `N[,M]` (1-based) -> the 0-based indexes; `count` (the live number of displays) bounds them when known.
pub fn display_selection(s: &str, count: Option<usize>) -> Result<Option<Vec<usize>>, String> {
    if s.is_empty() || s == "all" { return Ok(None); }
    s.split(',').map(|n| n.trim().parse::<usize>().ok().filter(|n| *n >= 1 && count.is_none_or(|c| *n <= c)).map(|n| n - 1)
        .ok_or_else(|| match count { Some(c) => format!("KFM_FLOW_DISPLAY: {n:?} is not 1..{c} (this computer has {c} display(s))"), None => format!("KFM_FLOW_DISPLAY must be all or N[,M] (1-based), got {s:?}") }))
        .collect::<Result<Vec<_>, _>>().map(Some)
}

// ---- whole-config validation ----

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Malformed: nothing may be written.
    pub errors: Vec<String>,
    /// Empty but needed by the role: `validate` / `apply` refuse, editing continues.
    pub missing: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn writable(&self) -> bool { self.errors.is_empty() }
    pub fn complete(&self) -> bool { self.errors.is_empty() && self.missing.is_empty() }
    pub fn summary(&self) -> String {
        let mut out = vec![];
        for (label, list) in [("ERROR", &self.errors), ("MISSING", &self.missing), ("warning", &self.warnings)] {
            for m in list { out.push(format!("  {label}: {m}")); }
        }
        if out.is_empty() { "  valid: no problems found".into() } else { out.join("\n") }
    }
}

/// Validate `settings` (and, when given, the keymap) as a whole. `displays` are this computer's live displays, if known.
pub fn check_settings(s: &Settings, keymap: Option<&str>, displays: Option<&[Rect]>) -> Report {
    let mut r = Report::default();
    for key in KEYS { if let Err(e) = check_value(key, s.get(key)) { r.errors.push(e); } }
    if let Some(text) = keymap && let Err(e) = remap::parse(text) { r.errors.push(e); }
    if s.get("KFM_KEYBOARD_VENDOR_ID").is_empty() != s.get("KFM_KEYBOARD_PRODUCT_ID").is_empty() {
        r.errors.push("set both KFM_KEYBOARD_VENDOR_ID and KFM_KEYBOARD_PRODUCT_ID, or neither".into());
    }
    let Some(role) = s.role() else {
        if s.get("KFM_ROLE").is_empty() { r.missing.push("KFM_ROLE (mac-source | receiver | windows-source)".into()); }
        return r;
    };
    for (key, what) in [("KFM_KEY", "the pre-shared key file"), ("KFM_PEER", "the other computer's IP:PORT"), ("KFM_BIND", "this computer's IP:PORT")] {
        if s.get(key).is_empty() { r.missing.push(format!("{key} ({what})")); }
    }
    for key in KEYS.iter().filter(|k| !role.uses(k) && !s.get(k).is_empty()) {
        // KFM_KEYMAP is filled in by `init` for every role; only a keyboard id or target OS is worth a warning.
        if *key != "KFM_KEYMAP" { r.warnings.push(format!("{key} is set but the {} role does not use it", role.as_str())); }
    }
    if !s.get("KFM_PEER").is_empty() && s.get("KFM_PEER") == s.get("KFM_BIND") { r.warnings.push("KFM_PEER and KFM_BIND are the same address".into()); }
    if role == Role::WindowsSource && !s.get("KFM_FLOW_DISPLAY").is_empty() && s.get("KFM_FLOW_EDGE").is_empty() {
        r.errors.push("KFM_FLOW_DISPLAY needs KFM_FLOW_EDGE for the windows-source role".into());
    }
    if let Some(ds) = displays { r.warnings.extend(layout_warnings(s, ds)); }
    r
}

/// Warnings (never errors) about KFM_FLOW_EDGE / KFM_FLOW_DISPLAY against this computer's live displays.
pub fn layout_warnings(s: &Settings, displays: &[Rect]) -> Vec<String> {
    let mut w = vec![];
    let Ok(edge) = s.get("KFM_FLOW_EDGE").parse::<Edge>() else { return w };
    if displays.is_empty() { return vec!["no displays were found on this computer, so KFM_FLOW_EDGE could not be checked".into()]; }
    let chosen: Vec<usize> = match display_selection(s.get("KFM_FLOW_DISPLAY"), Some(displays.len())) {
        Ok(None) => (0..displays.len()).collect(),
        Ok(Some(v)) => v,
        Err(e) => return vec![e],
    };
    let name = s.get("KFM_FLOW_EDGE");
    let shares: Vec<u32> = chosen.iter().map(|i| outer_edge_share(displays, *i, edge)).collect();
    let explicit = !matches!(display_selection(s.get("KFM_FLOW_DISPLAY"), None), Ok(None));
    if shares.iter().all(|s| *s == 0) {
        w.push(format!("the {name} edge is not an outer edge of the chosen display(s) {}: it only borders other displays, so the mouse cannot leave this computer there (see `kfm-config layout displays`)",
            chosen.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(",")));
    } else if explicit {
        // With `all`, inner displays naturally have no outer edge; only an explicit list is checked one by one.
        for (i, share) in chosen.iter().zip(&shares) {
            if *share == 0 { w.push(format!("display {}: its {name} edge borders another display (not an outer edge)", i + 1)); }
            else if *share < 100 { w.push(format!("display {}: only {share}% of its {name} edge is an outer edge", i + 1)); }
        }
    }
    w
}

/// Lines of `kfm-receive --list-displays`: `display N x=.. y=.. w=.. h=.. primary=true|false`.
pub fn parse_display_list(text: &str) -> Vec<Rect> {
    text.lines().filter_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()? == "display").then_some(())?;
        it.next()?;
        let mut v = [0f64; 4];
        for (slot, key) in v.iter_mut().zip(["x=", "y=", "w=", "h="]) { *slot = it.next()?.strip_prefix(key)?.parse().ok()?; }
        Some(Rect { x: v[0], y: v[1], w: v[2], h: v[3] })
    }).collect()
}

// ---- masking and plain-words output ----

pub fn mask_addr(s: &str) -> String {
    match s.parse::<SocketAddr>() {
        Ok(SocketAddr::V4(a)) => format!("{}.x.x.x:{}", a.ip().octets()[0], a.port()),
        Ok(SocketAddr::V6(a)) => format!("[x:x:...]:{}", a.port()),
        Err(_) if s.is_empty() => "(not set)".into(),
        Err(_) => "(set, hidden)".into(),
    }
}

fn mask_path(s: &str) -> String { if s.is_empty() { "(not set)".into() } else { "(set, hidden)".into() } }

/// `KEY = value` lines for `show` and the dry-run preview; addresses and paths are masked unless `reveal`.
pub fn settings_lines(s: &Settings, reveal: bool) -> Vec<String> {
    KEYS.iter().map(|k| {
        let v = s.get(k);
        let shown = if reveal { if v.is_empty() { "(not set)".to_string() } else { v.to_string() } } else {
            match *k { "KFM_PEER" | "KFM_BIND" => mask_addr(v), "KFM_KEY" | "KFM_KEYMAP" => mask_path(v), _ => if v.is_empty() { "(not set)".to_string() } else { v.to_string() } }
        };
        format!("{k} = {shown}")
    }).collect()
}

pub fn describe_settings(s: &Settings, reveal: bool, keymap_is_working_copy: bool) -> String {
    let d = |k: &str, none: &str| if s.get(k).is_empty() { none.to_string() } else { s.get(k).to_string() };
    let addr = |k: &str| if reveal { d(k, "(not set)") } else { mask_addr(s.get(k)) };
    let role = match s.role() {
        Some(Role::MacSource) => "mac-source (this Mac sends the keyboard; kfm-mac + Karabiner-Elements)",
        Some(Role::Receiver) => "receiver (this computer types what the source sends; kfm-receive)",
        Some(Role::WindowsSource) => "windows-source, EXPERIMENTAL (this Windows PC sends; captures every keyboard while forwarding)",
        None => "(not set)",
    };
    let mut o = vec![format!("Role: {role}")];
    o.push(format!("Other computer (KFM_PEER): {}", addr("KFM_PEER")));
    o.push(format!("This computer (KFM_BIND): {}", addr("KFM_BIND")));
    o.push(format!("Key file (KFM_KEY): {}", if reveal { d("KFM_KEY", "(not set)") } else { mask_path(s.get("KFM_KEY")) }));
    o.push(format!("Layout: this computer's {} edge of {} faces the other computer", d("KFM_FLOW_EDGE", "(default)"),
        match s.get("KFM_FLOW_DISPLAY") { "" | "all" => "all displays".to_string(), l => format!("display {l}") }));
    o.push(format!("Link timeout: {}", if s.get("KFM_LINK_TIMEOUT").is_empty() { "default (300 ms)".to_string() } else { format!("{} ms", s.get("KFM_LINK_TIMEOUT")) }));
    o.push(format!("Receiver OS (for the source's key mapping): {}", d("KFM_TARGET_OS", "windows (default)")));
    o.push(format!("Keyboard: {}", if s.get("KFM_KEYBOARD_VENDOR_ID").is_empty() { "the tested Rainy75".to_string() } else { format!("vendor {} product {}", s.get("KFM_KEYBOARD_VENDOR_ID"), s.get("KFM_KEYBOARD_PRODUCT_ID")) }));
    o.push(format!("Keymap file (KFM_KEYMAP): {}", match (s.get("KFM_KEYMAP").is_empty(), keymap_is_working_copy) {
        (true, _) => "(not set: defaults)".to_string(), (false, true) => "the keymap.json next to config.sh".to_string(),
        (false, false) => if reveal { s.get("KFM_KEYMAP").to_string() } else { "a file elsewhere (hidden)".to_string() } }));
    o.join("\n")
}

fn name(u: crate::keys::Usage) -> &'static str { name_of(u).unwrap_or("?") }

/// Plain-words lines for a keymap (also used to preview a keymap change).
pub fn keymap_lines(c: &KeyConfig) -> Vec<String> {
    let mut o = vec![format!("switch hotkey: {}{}", c.hotkey, if c.hotkey == "f13" { " (default)" } else { "" })];
    for (t, list) in [("windows", &c.windows), ("mac", &c.mac)] {
        if list.is_empty() { o.push(format!("target {t}: no remaps")); }
        for (from, a) in list {
            o.push(match a { Action::Key(to) => format!("target {t}: {} sends {}", name(*from), name(*to)), _ => format!("target {t}: {} is dropped (does nothing)", name(*from)) });
        }
    }
    o
}

// ---- keymap editing ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyOp {
    Map { target: TargetOs, from: String, to: String },
    Drop { target: TargetOs, from: String },
    Unmap { target: TargetOs, from: String },
    /// Both directions at once (a one-way map would leave two keys sending the same key).
    Swap { target: TargetOs, a: String, b: String },
    Hotkey(String),
    ResetTarget(TargetOs),
    ResetAll,
}

fn names_of(list: &[(crate::keys::Usage, Action)]) -> Vec<(String, String)> {
    list.iter().map(|(f, a)| (name(*f).to_string(), match a { Action::Key(t) => name(*t).to_string(), _ => "drop".to_string() })).collect()
}

fn put(l: &mut Vec<(String, String)>, from: &str, to: &str) {
    match l.iter_mut().find(|(f, _)| f == from) { Some(e) => e.1 = to.to_string(), None => l.push((from.to_string(), to.to_string())) }
}

fn known_key(n: &str) -> Result<String, String> {
    let n = n.trim().to_ascii_lowercase();
    if usage_of(&n).is_some() { return Ok(n); }
    let near: Vec<&str> = remap::forwarded_names().filter(|k| k.contains(n.as_str()) || n.contains(k) && k.len() > 3).take(6).collect();
    Err(format!("{n:?} is not a forwarded key{} (list them with `kfm-config keys`)", if near.is_empty() { String::new() } else { format!("; similar: {}", near.join(", ")) }))
}

/// The canonical keymap file text for `c` (`{}` when everything is default).
pub fn render_keymap(c: &KeyConfig) -> String {
    let q = |s: &str| serde_json::to_string(s).expect("string");
    let mut blocks = vec![];
    if c.hotkey != KeyConfig::default().hotkey { blocks.push(format!("  \"hotkey\": {}", q(&c.hotkey))); }
    let mut targets = vec![];
    for (t, list) in [("windows", names_of(&c.windows)), ("mac", names_of(&c.mac))] {
        if list.is_empty() { continue; }
        let pairs: Vec<String> = list.iter().map(|(f, to)| format!("      [{}, {}]", q(f), q(to))).collect();
        targets.push(format!("    {}: [\n{}\n    ]", q(t), pairs.join(",\n")));
    }
    if !targets.is_empty() { blocks.push(format!("  \"remap\": {{\n{}\n  }}", targets.join(",\n"))); }
    if blocks.is_empty() { "{}\n".into() } else { format!("{{\n{}\n}}\n", blocks.join(",\n")) }
}

/// Apply one edit to the keymap text and validate the result with `remap::parse` (all the rules live there).
pub fn apply_key_op(text: &str, op: &KeyOp) -> Result<String, String> {
    let c = remap::parse(text)?;
    let mut hotkey = c.hotkey.clone();
    let mut lists = [names_of(&c.windows), names_of(&c.mac)];
    let ix = |t: TargetOs| match t { TargetOs::Windows => 0, TargetOs::Mac => 1 };
    match op {
        KeyOp::Map { target, from, to } => {
            let (f, t) = (known_key(from)?, if to.eq_ignore_ascii_case("drop") { "drop".to_string() } else { known_key(to)? });
            if f == t { return Err(format!("mapping {f} to itself does nothing; use `unmap` to remove a mapping")); }
            put(&mut lists[ix(*target)], &f, &t);
        }
        KeyOp::Drop { target, from } => put(&mut lists[ix(*target)], &known_key(from)?, "drop"),
        KeyOp::Swap { target, a, b } => {
            let (a, b) = (known_key(a)?, known_key(b)?);
            if a == b { return Err("swap needs two different keys".into()); }
            put(&mut lists[ix(*target)], &a, &b);
            put(&mut lists[ix(*target)], &b, &a);
        }
        KeyOp::Unmap { target, from } => {
            let f = known_key(from)?;
            let l = &mut lists[ix(*target)];
            let before = l.len();
            l.retain(|(k, _)| *k != f);
            if l.len() == before { return Err(format!("{f} has no mapping for target {}", target_name(*target))); }
        }
        KeyOp::Hotkey(h) => {
            let h = h.trim().to_ascii_lowercase();
            if !EXTRA_HOTKEYS.contains(&h.as_str()) { return Err(format!("the hotkey must be one of f13..f20 (an ordinary key would stop working on the source Mac too), got {h:?}")); }
            hotkey = h;
        }
        KeyOp::ResetTarget(t) => lists[ix(*t)].clear(),
        KeyOp::ResetAll => { hotkey = KeyConfig::default().hotkey; lists[0].clear(); lists[1].clear(); }
    }
    let to_cfg = |l: &[(String, String)]| l.iter().map(|(f, t)| (usage_of(f).expect("known"), if t == "drop" { Action::Drop } else { Action::Key(usage_of(t).expect("known")) })).collect::<Vec<_>>();
    let new = render_keymap(&KeyConfig { hotkey, windows: to_cfg(&lists[0]), mac: to_cfg(&lists[1]) });
    remap::parse(&new).map_err(|e| format!("{e}\n  (two keys may not end up as the same key: to exchange two keys use `swap <target> a b`; to turn one key into another that exists, `drop` or `map` the other one first)"))?;
    Ok(new)
}

pub fn target_name(t: TargetOs) -> &'static str { match t { TargetOs::Windows => "windows", TargetOs::Mac => "mac" } }

// ---- files, backups, atomic writes ----

#[derive(Clone, Debug)]
pub struct Workspace {
    /// `<repo>/local` (or `--dir`).
    pub dir: PathBuf,
    /// Where install.sh puts the installed copies (macOS: `~/Library/Application Support/keyboard-follows-mouse`).
    pub installed: Option<PathBuf>,
}

impl Workspace {
    pub fn config_path(&self) -> PathBuf { self.dir.join("config.sh") }
    pub fn keymap_path(&self) -> PathBuf { self.dir.join("keymap.json") }
    pub fn backups_dir(&self) -> PathBuf { self.dir.join("backups") }
    pub fn validated_path(&self) -> PathBuf { self.dir.join(".validated") }
    pub fn read_config(&self) -> Result<Option<String>, String> { read_opt(&self.config_path()) }
    pub fn read_keymap(&self) -> Result<Option<String>, String> { read_opt(&self.keymap_path()) }
}

pub fn default_installed_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") { home_dir().map(|h| Path::new(&h).join("Library/Application Support/keyboard-follows-mouse")) } else { None }
}

fn read_opt(p: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(p) { Ok(t) => Ok(Some(t)), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None), Err(e) => Err(format!("{}: {e}", p.display())) }
}

/// Temp file in the same directory, then rename over the target.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_file_name(format!(".{}.kfm-tmp", path.file_name().map_or("file".into(), |n| n.to_string_lossy())));
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = res { let _ = std::fs::remove_file(&tmp); return Err(format!("{}: {e}", path.display())); }
    Ok(())
}

/// `YYYYMMDD-HHMMSS` (UTC) for Unix seconds; no `:` so it is a valid Windows folder name.
pub fn utc_stamp(secs: u64) -> String {
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn now_secs() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) }

/// Copy the current config.sh / keymap.json (those that exist) to `backups/<timestamp>/`. None when there is nothing to back up.
pub fn backup_current(ws: &Workspace) -> Result<Option<PathBuf>, String> {
    let files: Vec<PathBuf> = [ws.config_path(), ws.keymap_path()].into_iter().filter(|p| p.exists()).collect();
    if files.is_empty() { return Ok(None); }
    let stamp = utc_stamp(now_secs());
    let mut dir = ws.backups_dir().join(&stamp);
    for n in 1.. { if !dir.exists() { break; } dir = ws.backups_dir().join(format!("{stamp}-{n}")); }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for f in files { std::fs::copy(&f, dir.join(f.file_name().expect("file"))).map_err(|e| format!("backup of {}: {e}", f.display()))?; }
    Ok(Some(dir))
}

pub fn list_backups(ws: &Workspace) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(ws.backups_dir()).into_iter().flatten().flatten()
        .filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    // "20261111-113413" before "20261111-113413-1" before "...-2" ... "-10" (a number, not text).
    v.sort_by_key(|n| (n.chars().take(15).collect::<String>(), n.get(16..).and_then(|x| x.parse::<u32>().ok()).unwrap_or(0)));
    v
}

/// One planned change to the pair of files; nothing is written until `commit`.
#[derive(Clone, Debug)]
pub struct Plan {
    pub config_before: Option<String>,
    pub config_after: Option<String>,
    pub keymap_before: Option<String>,
    pub keymap_after: Option<String>,
    pub report: Report,
    /// config.sh keys named on the command line (shown in the preview even when unchanged).
    pub asked: Vec<String>,
    /// Extra lines for the preview (for example a relative path that was resolved).
    pub notes: Vec<String>,
}

fn keymap_view(text: &Option<String>) -> Vec<String> { remap::parse(text.as_deref().unwrap_or("{}")).map(|c| keymap_lines(&c)).unwrap_or_else(|e| vec![format!("(invalid: {e})")]) }
fn config_view(text: &Option<String>, reveal: bool) -> Vec<String> { text.as_deref().map(|t| Settings::parse(t).map(|s| settings_lines(&s, reveal)).unwrap_or_else(|e| vec![format!("(invalid: {e})")])).unwrap_or_default() }

impl Plan {
    fn build(config_before: Option<String>, config_after: Option<String>, keymap_before: Option<String>, keymap_after: Option<String>, displays: Option<&[Rect]>) -> Plan {
        let report = match &config_after {
            Some(c) => match Settings::parse(c) { Ok(s) => check_settings(&s, keymap_after.as_deref(), displays), Err(e) => Report { errors: vec![e], ..Default::default() } },
            None => Report { errors: remap::parse(keymap_after.as_deref().unwrap_or("{}")).err().into_iter().collect(), ..Default::default() },
        };
        Plan { config_before, config_after, keymap_before, keymap_after, report, asked: vec![], notes: vec![] }
    }

    /// Set one or more config.sh keys. `config.sh` must exist (`init` first).
    pub fn set_values(ws: &Workspace, pairs: &[(String, String)], displays: Option<&[Rect]>) -> Result<Plan, String> {
        let before = ws.read_config()?.ok_or_else(|| format!("{} does not exist: run `kfm-config init --role <mac-source|receiver|windows-source>` first", ws.config_path().display()))?;
        let mut s = Settings::parse(&before).map_err(|e| format!("{}: {e} (fix the file, or `kfm-config restore --list`)", ws.config_path().display()))?;
        let (mut notes, cwd) = (vec![], std::env::current_dir().ok());
        for (k, v) in pairs {
            let mut v = match v.strip_prefix("~/") { Some(rest) if matches!(k.as_str(), "KFM_KEY" | "KFM_KEYMAP") => format!("{}/{rest}", home_dir().ok_or("`~` used but HOME is not set")?.trim_end_matches(['/', '\\'])), _ => v.clone() };
            if matches!(k.as_str(), "KFM_KEY" | "KFM_KEYMAP") && !v.is_empty() && !is_absolute_like(&v) {
                let abs = resolve_path(&v, cwd.as_deref().ok_or("cannot resolve a relative path: the current folder is unknown")?);
                notes.push(format!("{k}: the relative path {v:?} was resolved against the current folder to {abs}"));
                v = abs;
            }
            check_value(k, &v)?;
            s.set(k, &v);
        }
        let after = s.render()?;
        let keymap = ws.read_keymap()?;
        let mut plan = Plan::build(Some(before), Some(after), keymap.clone(), keymap, displays);
        plan.asked = pairs.iter().map(|(k, _)| k.clone()).collect();
        plan.notes = notes;
        Ok(plan)
    }

    pub fn key_edit(ws: &Workspace, op: &KeyOp, displays: Option<&[Rect]>) -> Result<Plan, String> {
        let keymap_before = ws.read_keymap()?;
        let after = apply_key_op(keymap_before.as_deref().unwrap_or("{}"), op)?;
        let config = ws.read_config()?;
        Ok(Plan::build(config.clone(), config, keymap_before, Some(after), displays))
    }

    pub fn changed(&self) -> bool { self.config_before != self.config_after || self.keymap_before != self.keymap_after }

    /// Before -> after, in plain words, and the validation result.
    pub fn preview(&self, reveal: bool) -> String {
        let mut o = vec![];
        let mut diff = |label: &str, a: Vec<String>, b: Vec<String>| {
            let (gone, new): (Vec<_>, Vec<_>) = (a.iter().filter(|l| !b.contains(l)).collect(), b.iter().filter(|l| !a.contains(l)).collect());
            if gone.is_empty() && new.is_empty() { return; }
            o.push(format!("{label}:"));
            for l in gone { o.push(format!("  - {l}")); }
            for l in new { o.push(format!("  + {l}")); }
        };
        diff("config.sh", config_view(&self.config_before, reveal), config_view(&self.config_after, reveal));
        diff("keymap.json", keymap_view(&self.keymap_before), keymap_view(&self.keymap_after));
        let (cb, ca) = (config_view(&self.config_before, reveal), config_view(&self.config_after, reveal));
        let unchanged: Vec<String> = self.asked.iter().filter_map(|k| ca.iter().find(|l| l.starts_with(&format!("{k} = "))).filter(|l| cb.contains(l)).cloned()).collect();
        if !unchanged.is_empty() && !o.iter().any(|l| l == "config.sh:") { o.push("config.sh:".into()); }
        let at = o.iter().position(|l| l == "config.sh:").map_or(o.len(), |i| o[i + 1..].iter().position(|l| !l.starts_with("  ")).map_or(o.len(), |n| i + 1 + n));
        for (n, l) in unchanged.into_iter().enumerate() { o.insert(at + n, format!("  = {l} (unchanged)")); }
        if o.is_empty() { o.push("no change".into()); }
        o.extend(self.notes.iter().map(|n| format!("note: {n}")));
        o.push("validation:".into());
        o.push(self.report.summary());
        if let (Ok(a), Ok(b)) = (remap::parse(self.keymap_before.as_deref().unwrap_or("{}")), remap::parse(self.keymap_after.as_deref().unwrap_or("{}")))
            && a.hotkey != b.hotkey {
            o.push("note: the hotkey changed, so the Karabiner rule must be applied again (`kfm-config apply` shows the step)".into());
        }
        o.join("\n")
    }

    /// Validate, back up the previous files, then write atomically. Refuses (writing nothing) when invalid.
    /// Returns the backup folder, if one was made.
    pub fn commit(&self, ws: &Workspace) -> Result<Option<PathBuf>, String> {
        if !self.report.writable() { return Err(format!("not written; the result would be invalid:\n{}", self.report.summary())); }
        if !self.changed() { return Ok(None); }
        std::fs::create_dir_all(&ws.dir).map_err(|e| format!("{}: {e}", ws.dir.display()))?;
        let backup = backup_current(ws)?;
        if self.config_before != self.config_after && let Some(c) = &self.config_after { write_atomic(&ws.config_path(), c)?; }
        if self.keymap_before != self.keymap_after && let Some(k) = &self.keymap_after { write_atomic(&ws.keymap_path(), k)?; }
        Ok(backup)
    }
}

/// `init`: config.sh from the template (and an empty keymap), refusing to overwrite either.
pub fn init(ws: &Workspace, role: Role) -> Result<Vec<PathBuf>, String> {
    if ws.config_path().exists() { return Err(format!("{} already exists; nothing was changed. Edit it with `kfm-config set`, or move it away first", ws.config_path().display())); }
    std::fs::create_dir_all(&ws.dir).map_err(|e| format!("{}: {e}", ws.dir.display()))?;
    let text = Settings::template(role, &ws.dir).render()?;
    write_atomic(&ws.config_path(), &text)?;
    let mut made = vec![ws.config_path()];
    if !ws.keymap_path().exists() { write_atomic(&ws.keymap_path(), "{}\n")?; made.push(ws.keymap_path()); }
    Ok(made)
}

pub fn restore(ws: &Workspace, stamp: &str) -> Result<(Option<PathBuf>, Vec<String>), String> {
    if stamp.is_empty() || stamp.contains(['/', '\\']) || stamp.starts_with('.') { return Err(format!("{stamp:?} is not a backup name (see `kfm-config restore --list`)")); }
    let dir = ws.backups_dir().join(stamp);
    if !dir.is_dir() { return Err(format!("no backup {stamp:?} (see `kfm-config restore --list`)")); }
    let (c, k) = (read_opt(&dir.join("config.sh"))?, read_opt(&dir.join("keymap.json"))?);
    if c.is_none() && k.is_none() { return Err(format!("backup {stamp} holds no files")); }
    if let Some(t) = &c { let s = Settings::parse(t).map_err(|e| format!("backup config.sh is invalid ({e}); not restored"))?; let r = check_settings(&s, None, None); if !r.writable() { return Err(format!("backup config.sh is invalid; not restored:\n{}", r.summary())); } }
    if let Some(t) = &k { remap::parse(t).map_err(|e| format!("backup keymap.json is invalid ({e}); not restored"))?; }
    let backup = backup_current(ws)?;
    let mut done = vec![];
    if let Some(t) = c { write_atomic(&ws.config_path(), &t)?; done.push("config.sh".into()); }
    if let Some(t) = k { write_atomic(&ws.keymap_path(), &t)?; done.push("keymap.json".into()); }
    Ok((backup, done))
}

// ---- fingerprints and status ----

/// FNV-1a 64: change detection for `.validated`, not a security hash.
pub fn fingerprint(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() { h ^= u64::from(b); h = h.wrapping_mul(0x100000001b3); }
    format!("{h:016x}")
}

fn fp_or_absent(t: &Option<String>) -> String { t.as_deref().map_or("absent".into(), fingerprint) }

/// Validate both files; on success record their fingerprints in `.validated`.
pub fn validate(ws: &Workspace, displays: Option<&[Rect]>) -> Result<Report, String> {
    let config = ws.read_config()?.ok_or_else(|| format!("{} does not exist: run `kfm-config init --role ...` first", ws.config_path().display()))?;
    let keymap = ws.read_keymap()?;
    let report = match Settings::parse(&config) {
        Ok(s) => {
            let mut r = check_settings(&s, Some(keymap.as_deref().unwrap_or("{}")), displays);
            if !s.get("KFM_KEYMAP").is_empty() && Path::new(s.get("KFM_KEYMAP")) != ws.keymap_path() {
                r.warnings.push(format!("KFM_KEYMAP points to {} but kfm-config edits {}; set KFM_KEYMAP to the second one", s.get("KFM_KEYMAP"), ws.keymap_path().display()));
            }
            r
        }
        Err(e) => Report { errors: vec![e], ..Default::default() },
    };
    if report.complete() { write_atomic(&ws.validated_path(), &format!("config={}\nkeymap={}\n", fingerprint(&config), fp_or_absent(&keymap)))?; }
    Ok(report)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Running { Unknown, NotRunning, Since(std::time::SystemTime) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State { Missing, Edited, Validated, Applied }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStatus { pub file: &'static str, pub state: State, pub note: String }

/// Effective values of the keys install.sh writes (its defaults filled in), for comparing local and installed copies.
fn effective(s: &Settings) -> Vec<String> {
    INSTALLED_KEYS.iter().map(|k| {
        let v = s.get(k);
        match (*k, v.is_empty()) { ("KFM_FLOW_EDGE", true) => "right".into(), ("KFM_FLOW_DISPLAY", true) => "all".into(), ("KFM_TARGET_OS", true) => "windows".into(), _ => v.to_string() }
    }).collect()
}

pub fn config_applied(local: &Settings, installed_text: &str) -> bool { Settings::parse(installed_text).is_ok_and(|i| effective(&i) == effective(local)) }

/// A missing installed keymap equals the default one (install.sh removes it when KFM_KEYMAP is empty).
pub fn keymap_applied(local_text: Option<&str>, installed_text: Option<&str>) -> bool {
    match (remap::parse(local_text.unwrap_or("{}")), remap::parse(installed_text.unwrap_or("{}"))) { (Ok(a), Ok(b)) => a == b, _ => false }
}

/// Parse `ps -o etime=` (`[[dd-]hh:]mm:ss`) into seconds.
pub fn parse_etime(s: &str) -> Option<u64> {
    let s = s.trim();
    let (days, rest) = match s.split_once('-') { Some((d, r)) => (d.parse::<u64>().ok()?, r), None => (0, s) };
    let parts: Vec<u64> = rest.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let secs = match parts.as_slice() { [m, s] => m * 60 + s, [h, m, s] => h * 3600 + m * 60 + s, _ => return None };
    Some(days * 86400 + secs)
}

pub fn status(ws: &Workspace, running: Running) -> Result<Vec<FileStatus>, String> {
    let (config, keymap) = (ws.read_config()?, ws.read_keymap()?);
    let validated = read_opt(&ws.validated_path())?.unwrap_or_default();
    let recorded = |k: &str| validated.lines().find_map(|l| l.strip_prefix(&format!("{k}=")).map(String::from));
    let installed_for = |name: &str| ws.installed.as_ref().map(|d| d.join(name));
    let mtime = |p: &Option<PathBuf>| p.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    let restart_note = |p: &Option<PathBuf>| match (running, mtime(p)) {
        (Running::NotRunning, _) => "; kfm-mac is not running".to_string(),
        (Running::Since(start), Some(m)) if m > start => "; RESTART NEEDED: the installed copy is newer than the running kfm-mac".to_string(),
        (Running::Since(_), _) => "; the running kfm-mac started after it".to_string(),
        (Running::Unknown, _) => "; restart to be sure the running kfm-mac uses it".to_string(),
    };
    let mut out = vec![];
    // config.sh
    out.push(match &config {
        None => FileStatus { file: "config.sh", state: State::Missing, note: "run `kfm-config init --role ...`".into() },
        Some(text) => {
            let p = installed_for("config.sh");
            let applied = ws.installed.as_ref().is_some_and(|_| match (Settings::parse(text), p.as_ref().and_then(|p| read_opt(p).ok().flatten())) { (Ok(s), Some(i)) => config_applied(&s, &i), _ => false });
            if applied { FileStatus { file: "config.sh", state: State::Applied, note: format!("identical to the installed copy (compared: the keys install.sh writes){}", restart_note(&p)) } }
            else if recorded("config") == Some(fingerprint(text)) { FileStatus { file: "config.sh", state: State::Validated, note: "not applied: the installed copy differs or is absent".into() } }
            else { FileStatus { file: "config.sh", state: State::Edited, note: "changed since the last `kfm-config validate`".into() } }
        }
    });
    // keymap.json
    out.push(match &keymap {
        None if config.is_none() => FileStatus { file: "keymap.json", state: State::Missing, note: "defaults apply".into() },
        _ => {
            let p = installed_for("keymap.json");
            let installed = p.as_ref().and_then(|p| read_opt(p).ok().flatten());
            let applied = ws.installed.is_some() && keymap_applied(keymap.as_deref(), installed.as_deref());
            if applied { FileStatus { file: "keymap.json", state: State::Applied, note: format!("{}{}", if installed.is_some() { "same keys as the installed copy" } else { "default keys; no installed keymap is needed" }, restart_note(&p)) } }
            else if recorded("keymap") == Some(fp_or_absent(&keymap)) { FileStatus { file: "keymap.json", state: State::Validated, note: "not applied: the installed copy differs or is absent".into() } }
            else { FileStatus { file: "keymap.json", state: State::Edited, note: "changed since the last `kfm-config validate`".into() } }
        }
    });
    Ok(out)
}

/// What `kfm-karabiner preflight` says about our rule (its stdout is parsed; the file it reads is Karabiner's).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleState {
    /// Installed, and both its hotkey and its keyboard match the configuration.
    Current,
    /// Installed but stale: apply the rule again. `reason` says what differs.
    Differs { reason: String },
    NotInstalled,
    /// preflight did not say (a line is missing or unparseable): never treated as current.
    Unknown,
}

fn preflight_pair<'a>(out: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    out.lines().find_map(|l| l.strip_prefix(prefix)).and_then(|l| l.split_once(", installed rule "))
}

pub fn rule_state(preflight_stdout: &str) -> RuleState {
    if preflight_stdout.contains("(ours at None)") { return RuleState::NotInstalled; }
    let (Some((hc, hi)), Some((dc, di))) = (preflight_pair(preflight_stdout, "hotkey: configured "), preflight_pair(preflight_stdout, "device: configured ")) else { return RuleState::Unknown };
    let mut reasons = vec![];
    if hc != hi { reasons.push(format!("hotkey: the installed rule uses {hi} but the keymap says {hc}")); }
    if dc != di || di.contains('?') { reasons.push(format!("keyboard: the installed rule is limited to {di} but KFM_KEYBOARD_VENDOR_ID/PRODUCT_ID say {dc} (unset = the tested Rainy75, 0x320f:0x5055)")); }
    if reasons.is_empty() { RuleState::Current } else { RuleState::Differs { reason: reasons.join("; ") } }
}

// ---- command lines for kfm-receive / kfm-send ----

pub fn run_args(s: &Settings, which: Role) -> Result<Vec<String>, String> {
    if s.role() != Some(which) { return Err(format!("KFM_ROLE is {:?}, not {}; this command is for the {} role (a mac-source uses scripts/resident/install.sh)", s.get("KFM_ROLE"), which.as_str(), which.as_str())); }
    let r = check_settings(s, None, None);
    if !r.complete() { return Err(format!("config.sh is not complete:\n{}", r.summary())); }
    let a = |v: &str| v.to_string();
    let disp = if s.get("KFM_FLOW_DISPLAY").is_empty() { "all" } else { s.get("KFM_FLOW_DISPLAY") };
    match which {
        Role::Receiver => Ok(vec![a("--bind"), a(s.get("KFM_BIND")), a("--peer"), a(s.get("KFM_PEER")), a("--key-file"), a(s.get("KFM_KEY")),
            a("--flow-edge"), a(if s.get("KFM_FLOW_EDGE").is_empty() { "left" } else { s.get("KFM_FLOW_EDGE") }), a("--flow-display"), a(disp)]),
        Role::WindowsSource => {
            let mut v = vec![a("--peer"), a(s.get("KFM_PEER")), a("--bind"), a(s.get("KFM_BIND")), a("--key-file"), a(s.get("KFM_KEY"))];
            if !s.get("KFM_LINK_TIMEOUT").is_empty() { v.extend([a("--link-timeout-ms"), a(s.get("KFM_LINK_TIMEOUT"))]); }
            if !s.get("KFM_FLOW_EDGE").is_empty() { v.extend([a("--flow-edge"), a(s.get("KFM_FLOW_EDGE")), a("--flow-display"), a(disp)]); }
            Ok(v)
        }
        Role::MacSource => Err("a mac-source runs through scripts/resident/install.sh, not run-args".into()),
    }
}

// ---- key file (stat only) ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyFile { Missing, NotAFile, Ok { mode_note: String }, BadMode(u32) }

/// Stat the key file; its content is never opened.
pub fn key_file_state(path: &str) -> KeyFile {
    let Ok(m) = std::fs::metadata(path) else { return KeyFile::Missing };
    if !m.is_file() { return KeyFile::NotAFile; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = m.permissions().mode() & 0o777;
        if mode & 0o077 != 0 { return KeyFile::BadMode(mode); }
        KeyFile::Ok { mode_note: format!("mode {mode:o}") }
    }
    #[cfg(not(unix))]
    { KeyFile::Ok { mode_note: "exists; the Windows ACL is not checked".into() } }
}

// ---- doctor ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level { Ok, Info, Warn, Problem }

#[derive(Clone, Debug)]
pub struct Check { pub level: Level, pub title: String, pub detail: String, pub cause: String, pub next: String, pub undo: String }

impl Check {
    fn ok(title: impl Into<String>, detail: impl Into<String>) -> Check { Check { level: Level::Ok, title: title.into(), detail: detail.into(), cause: String::new(), next: String::new(), undo: String::new() } }
    fn info(title: impl Into<String>, detail: impl Into<String>) -> Check { Check { level: Level::Info, ..Check::ok(title, detail) } }
    fn issue(level: Level, title: impl Into<String>, cause: &str, next: &str, undo: &str) -> Check {
        Check { level, title: title.into(), detail: String::new(), cause: cause.into(), next: next.into(), undo: undo.into() }
    }
    pub fn render(&self) -> String {
        let tag = match self.level { Level::Ok => "[ok]     ", Level::Info => "[info]   ", Level::Warn => "[warning]", Level::Problem => "[PROBLEM]" };
        let mut o = format!("{tag} {}{}", self.title, if self.detail.is_empty() { String::new() } else { format!(": {}", self.detail) });
        for (label, v) in [("cause", &self.cause), ("next ", &self.next), ("undo ", &self.undo)] { if !v.is_empty() { o.push_str(&format!("\n            {label}: {v}")); } }
        o
    }
}

/// Launchd jobs found on macOS (labels only).
#[derive(Clone, Debug, Default)]
pub struct Jobs { pub ours_loaded: Vec<String>, pub others: Vec<String> }

/// Facts the binary gathers from the OS; `doctor_checks` turns them into findings.
#[derive(Clone, Debug)]
pub struct Facts {
    pub os: &'static str,
    pub arch: &'static str,
    /// (binary name, built) for the binaries this OS can use.
    pub binaries: Vec<(String, bool)>,
    pub karabiner_installed: Option<bool>,
    /// Ok(stdout) when `kfm-karabiner preflight` passed, Err(output) when it ran and failed, None when it could not run.
    pub preflight: Option<Result<String, String>>,
    pub jobs: Option<Jobs>,
    pub displays: Option<Vec<Rect>>,
    pub repo_root: PathBuf,
}

pub fn doctor_checks(ws: &Workspace, f: &Facts) -> Vec<Check> {
    let mut c = vec![Check::ok("platform", format!("{} {}", f.os, f.arch))];
    // Before anything else, even without settings: another installation must stop the whole setup.
    if let Some(j) = &f.jobs {
        if !j.others.is_empty() { c.push(Check::issue(Level::Problem, format!("an existing installation was found: {}. Do not overwrite it", j.others.join(", ")), "another kfm/r75 job is loaded in launchd (an earlier install, possibly under the older r75 name)", "STOP: ask the user before asking setup questions or creating settings; do not touch these jobs, do not run install.sh", "none (kfm-config changed nothing)")); }
        if !j.ours_loaded.is_empty() { c.push(Check::issue(Level::Info, format!("resident already installed: {}", j.ours_loaded.join(", ")), "scripts/resident/install.sh was run before", "to change settings: uninstall (dry run, then --apply) and install again; `kfm-config apply` lists the steps", "`sh scripts/resident/uninstall.sh --apply`")); }
        if j.ours_loaded.is_empty() && j.others.is_empty() { c.push(Check::ok("launchd", "no kfm/r75 job loaded")); }
    }
    let missing: Vec<&str> = f.binaries.iter().filter(|(_, b)| !b).map(|(n, _)| n.as_str()).collect();
    if missing.is_empty() { c.push(Check::ok("release binaries", f.binaries.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ") + " are built")); }
    else { c.push(Check::issue(Level::Problem, format!("release binaries not built: {}", missing.join(", ")), "`cargo build --release` has not been run in this folder (or failed)",
        &format!("run the setup script, or `cargo build --release` in {}", f.repo_root.display()), "delete the target/ folder (build output only; nothing else changed)")); }

    let config = ws.read_config();
    let keymap = ws.read_keymap();
    let mut settings: Option<Settings> = None;
    match &config {
        Err(e) => c.push(Check::issue(Level::Problem, "config.sh unreadable", e, "check the file permissions", "none (nothing was changed)")),
        Ok(None) => c.push(Check::issue(Level::Problem, format!("{} does not exist yet", ws.config_path().display()), "settings have not been created",
            "`kfm-config init --role <mac-source|receiver|windows-source>`, then `kfm-config set KEY VALUE`", "delete the local/ folder (settings only)")),
        Ok(Some(t)) => match Settings::parse(t) {
            Err(e) => c.push(Check::issue(Level::Problem, "config.sh does not parse", &e, "fix the line, or `kfm-config restore --list`", "`kfm-config restore <timestamp>`")),
            Ok(s) => { c.push(Check::ok("config.sh", "parses")); settings = Some(s); }
        },
    }
    match &keymap {
        Err(e) => c.push(Check::issue(Level::Problem, "keymap.json unreadable", e, "check the file permissions", "none (nothing was changed)")),
        Ok(None) => c.push(Check::info("keymap.json", "absent: the default keys apply (hotkey F13, no remaps)")),
        Ok(Some(t)) => match remap::parse(t) {
            Err(e) => c.push(Check::issue(Level::Problem, "keymap.json is invalid", &e, "fix it with `kfm-config map/drop/unmap/hotkey/reset`, or `kfm-config restore --list`", "`kfm-config restore <timestamp>`")),
            Ok(_) => c.push(Check::ok("keymap.json", "valid")),
        },
    }
    let Some(s) = settings else { return c };
    let keymap_text = keymap.ok().flatten();
    let rep = check_settings(&s, keymap_text.as_deref().or(Some("{}")), f.displays.as_deref());
    for e in &rep.errors { c.push(Check::issue(Level::Problem, "invalid setting", e, "`kfm-config set KEY VALUE` with a valid value", "`kfm-config restore --list` and `restore <timestamp>`")); }
    for m in &rep.missing { c.push(Check::issue(Level::Problem, format!("not set yet: {m}"), "the value has not been entered", "`kfm-config set KEY VALUE`", "`kfm-config restore --list` and `restore <timestamp>`")); }
    for w in &rep.warnings { c.push(Check::issue(Level::Warn, w.clone(), "see the message", "decide whether it matches what you want", "`kfm-config restore <timestamp>`")); }
    if !s.get("KFM_KEYMAP").is_empty() && Path::new(s.get("KFM_KEYMAP")) != ws.keymap_path() {
        c.push(Check::issue(Level::Warn, "KFM_KEYMAP points to another file than the one kfm-config edits", "the setting was changed by hand or the folder moved",
            &format!("`kfm-config set KFM_KEYMAP {}`", ws.keymap_path().display()), "`kfm-config restore <timestamp>`"));
    }
    let Some(role) = s.role() else { return c };
    c.push(Check::info("role", role.as_str()));

    // the key file: stat only
    if !s.get("KFM_KEY").is_empty() {
        match key_file_state(s.get("KFM_KEY")) {
            KeyFile::Ok { mode_note } => c.push(Check::ok("key file", format!("present ({mode_note}); contents not read"))),
            KeyFile::BadMode(m) => c.push(Check::issue(Level::Problem, format!("key file mode is {m:o}, others can read it"), "the key must be owner-only", "`chmod 600` on the key file (kfm-config does not change permissions)", "none needed")),
            KeyFile::NotAFile => c.push(Check::issue(Level::Problem, "KFM_KEY is not a file", "the path is a folder or special file", "`kfm-config set KFM_KEY <path>`", "`kfm-config restore <timestamp>`")),
            KeyFile::Missing => c.push(Check::issue(Level::Problem, "key file does not exist", "no key has been made on this computer yet",
                "make it with `kfm-config keygen --out <KFM_KEY path>` (any OS, never overwrites, never prints it), then copy the same file to the other computer yourself over a channel you trust (never through chat); or copy the other computer's file here",
                "delete the key file (nothing else depends on it until a resident is installed)")),
        }
    }

    // role-specific
    let built = |n: &str| f.binaries.iter().any(|(b, ok)| b.trim_end_matches(".exe") == n && *ok);
    match role {
        Role::MacSource => {
            if f.os != "macos" { c.push(Check::issue(Level::Problem, "role mac-source needs macOS", &format!("this computer is {}", f.os), "use `--role receiver` here, or run the source role on the Mac", "`kfm-config restore <timestamp>`")); }
            else {
                match f.karabiner_installed {
                    Some(true) => c.push(Check::ok("Karabiner-Elements", "installed (path check only)")),
                    _ => c.push(Check::issue(Level::Problem, "Karabiner-Elements not found at /Applications/Karabiner-Elements.app", "it is not installed (or installed elsewhere)", "install it yourself from https://karabiner-elements.pqrs.org/ and grant the permissions it asks for; kfm-config will not", "uninstall it with its own uninstaller")),
                }
                match &f.preflight {
                    Some(Ok(out)) => {
                        c.push(Check::ok("kfm-karabiner preflight", "no problems"));
                        match rule_state(out) {
                            RuleState::NotInstalled => c.push(Check::issue(Level::Info, "the kfm Karabiner rule is not installed yet", "first setup", "`kfm-config apply` shows the `kfm-karabiner apply` step (it backs up karabiner.json first)", "`kfm-karabiner restore --backup <file> --confirm-restore`")),
                            RuleState::Differs { reason } => c.push(Check::issue(Level::Warn, format!("the installed Karabiner rule is out of date ({reason})"), "the hotkey or the keyboard id was changed after the rule was applied, so the old rule keeps capturing the old setting", "apply the rule again (`kfm-config apply` shows the command)", "`kfm-karabiner restore --backup <file> --confirm-restore`")),
                            RuleState::Current => c.push(Check::ok("Karabiner rule", "installed with the configured hotkey and keyboard")),
                            RuleState::Unknown => c.push(Check::issue(Level::Warn, "cannot tell whether the installed Karabiner rule matches the configured hotkey and keyboard", "kfm-karabiner preflight did not report them", "run `kfm-karabiner preflight` by hand; `kfm-config apply` includes the rule step", "none (read-only)")),
                        }
                    }
                    Some(Err(out)) => c.push(Check::issue(Level::Problem, "kfm-karabiner preflight reports problems", out.trim(), "read the problem list; fix it in Karabiner-Elements (kfm-config never edits Karabiner)", "none (preflight is read-only)")),
                    None => c.push(Check::info("kfm-karabiner preflight", "not run (binary not built, or karabiner.json not found)")),
                }
                for n in ["kfm-mac", "kfm-karabiner"] { if !built(n) && missing.is_empty() { c.push(Check::issue(Level::Problem, format!("{n} is not built"), "not part of this build", "`cargo build --release`", "delete target/")); } }
            }
        }
        Role::Receiver => if !built("kfm-receive") && missing.is_empty() { c.push(Check::issue(Level::Problem, "kfm-receive is not built", "not part of this build", "`cargo build --release`", "delete target/")); }
        Role::WindowsSource => {
            c.push(Check::info("EXPERIMENTAL", "windows-source captures EVERY keyboard on this PC while forwarding and has never run on real Windows hardware"));
            if f.os != "windows" { c.push(Check::issue(Level::Problem, "role windows-source needs Windows", &format!("this computer is {}", f.os), "run the source role on Windows, or use `--role receiver` here", "`kfm-config restore <timestamp>`")); }
        }
    }
    if f.displays.is_none() { c.push(Check::info("displays", "not listed (kfm-receive is not built); the layout was not checked against this computer's displays")); }
    c
}

// ---- apply plan ----

pub struct PlanCtx<'a> {
    pub settings: &'a Settings,
    pub ws: &'a Workspace,
    pub repo_root: &'a Path,
    pub shell: Shell,
    pub os: &'a str,
    pub rule: RuleState,
    pub jobs: Option<&'a Jobs>,
    pub key_exists: bool,
    pub validated: bool,
}

/// The message that replaces any install plan when another kfm/r75 installation is loaded.
pub fn foreign_stop_message(j: &Jobs) -> Option<String> {
    if j.others.is_empty() { return None; }
    Some(format!("STOP: an existing installation was found in launchd ({}). No install plan is printed.\n  - Do not overwrite it and do not run install.sh. Ask the user what to do before anything else.\n  - kfm-config changed nothing and will not touch these jobs. If the user wants them replaced, they remove the old installation themselves with the uninstaller it came with\n    (scripts/resident/uninstall.sh only handles local.kfm-mac and local.kfm-guard, and is a dry run until --apply), then run `kfm-config doctor` again.", j.others.join(", ")))
}

/// Text for the user to approve. Nothing here is executed.
pub fn apply_plan(p: &PlanCtx) -> Result<String, String> {
    if let Some(m) = p.jobs.and_then(foreign_stop_message) { return Err(m); }
    let s = p.settings;
    let role = s.role().ok_or("KFM_ROLE is not set")?;
    let r = check_settings(s, None, None);
    if !r.complete() { return Err(format!("config.sh is not complete; no plan:\n{}", r.summary())); }
    let sh = p.shell;
    let exe = |n: &str| p.repo_root.join("target/release").join(if cfg!(windows) { format!("{n}.exe") } else { n.to_string() }).to_string_lossy().into_owned();
    let mut o = vec![format!("Plan for role {} on {}. kfm-config does NOT run any of this: it needs your approval, step by step.", role.as_str(), p.os)];
    if !p.validated { o.push("note: config.sh or keymap.json changed since the last `kfm-config validate`; run it first.".into()); }
    match role {
        Role::MacSource => {
            if p.os != "macos" { return Err("the mac-source role applies on macOS only".into()); }
            let cfg = p.ws.config_path().to_string_lossy().into_owned();
            let install = p.repo_root.join("scripts/resident/install.sh").to_string_lossy().into_owned();
            let uninstall = p.repo_root.join("scripts/resident/uninstall.sh").to_string_lossy().into_owned();
            let mut n = 1;
            let mut step = |o: &mut Vec<String>, title: &str, lines: Vec<String>| { o.push(format!("\nStep {n}: {title}")); o.extend(lines.into_iter().map(|l| format!("  {l}"))); n += 1; };
            if !p.key_exists { step(&mut o, "make the pre-shared key (new file, mode 600)", vec![format!("run:      {}", sh.command(&exe("kfm-config"), &["keygen".into(), "--out".into(), s.get("KFM_KEY").into()])),
                "then copy that file to the receiver yourself (USB, AirDrop); never through chat".into(), format!("rollback: delete {}", s.get("KFM_KEY"))]); }
            let km = p.ws.keymap_path().to_string_lossy().into_owned();
            let kenv = format!("{}{}", sh.env_prefix("KFM_KEYMAP", &km), if s.get("KFM_KEYBOARD_VENDOR_ID").is_empty() { String::new() } else { format!("{}{}", sh.env_prefix("KFM_KEYBOARD_VENDOR_ID", s.get("KFM_KEYBOARD_VENDOR_ID")), sh.env_prefix("KFM_KEYBOARD_PRODUCT_ID", s.get("KFM_KEYBOARD_PRODUCT_ID"))) });
            let kconfig = "$HOME/.config/karabiner/karabiner.json";
            match &p.rule {
                RuleState::Current => o.push("\nKarabiner rule: already installed with this hotkey and keyboard; no step needed.".into()),
                other => {
                    let why = match other { RuleState::NotInstalled => "the kfm rule is not installed yet".to_string(), RuleState::Differs { reason } => reason.clone(), _ => "preflight could not tell whether the rule matches (run `kfm-karabiner preflight`)".to_string() };
                    step(&mut o, &format!("Karabiner rule ({why}); changes ~/.config/karabiner/karabiner.json"), vec![
                        format!("dry run:  {kenv}{} --out {}   # writes only that file; Karabiner untouched", sh.command(&exe("kfm-karabiner"), &["plan".into()]), sh.quote(&p.ws.dir.join("karabiner-plan.json").to_string_lossy())),
                        format!("apply:    {kenv}{} --config \"{kconfig}\" --confirm-backup-and-apply   # backs up first and prints the backup path", sh.command(&exe("kfm-karabiner"), &["apply".into()])),
                        format!("rollback: {} --config \"{kconfig}\" --backup <the path apply printed> --confirm-restore", sh.command(&exe("kfm-karabiner"), &["restore".into()])),
                    ]);
                }
            }
            let loaded = p.jobs.is_some_and(|j| !j.ours_loaded.is_empty());
            if loaded { step(&mut o, "a resident from this project is already installed: install.sh refuses to run over it", vec![
                format!("dry run:  sh {}", sh.quote(&uninstall)), format!("remove:   sh {} --apply   # stops kfm-mac (route 0, keys released), removes the LaunchAgents; files and logs stay", sh.quote(&uninstall)),
                "kfmctl.sh only has pause / resume / status: it cannot restart or reload settings, so changed settings need uninstall then install".into(),
                "rollback: reinstall with the previous settings (`kfm-config restore --list`), then the install step below".into()]); }
            let sourced = format!("( set -a; . {}; set +a; sh {} {{}} )", sh.quote(&cfg), sh.quote(&install));
            step(&mut o, "install the resident (two LaunchAgents, installed copies under ~/Library/Application Support/keyboard-follows-mouse)", vec![
                format!("dry run:  {}", sourced.replace(" {}", "")), format!("install:  {}", sourced.replace("{}", "--apply")),
                format!("rollback: sh {} (dry run), then sh {} --apply", sh.quote(&uninstall), sh.quote(&uninstall)),
                "install.sh stops by itself if the key is unreadable, the Karabiner rule is missing, or something is already running".into()]);
            step(&mut o, "check, pause or resume (nothing restarts; kfmctl has no restart)", vec!["sh \"$HOME/Library/Application Support/keyboard-follows-mouse/bin/kfmctl.sh\" status|pause|resume".into()]);
            if matches!(p.rule, RuleState::Differs { .. } | RuleState::NotInstalled | RuleState::Unknown) { o.push("\nOrder matters: the Karabiner rule step must be done BEFORE the install step, even before its dry run: install.sh checks that the kfm rule is installed first and matches the hotkey, and stops otherwise.".into()); }
        }
        Role::Receiver | Role::WindowsSource => {
            let args = run_args(s, role)?;
            if !p.key_exists { o.push(format!("\nStep 0: the key file does not exist yet. Either copy it from the other computer yourself (USB, AirDrop; never chat), or make it here and copy it back:\n  {}\n  rollback: delete that file", sh.command(&exe("kfm-config"), &["keygen".into(), "--out".into(), s.get("KFM_KEY").into()]))); }
            let bin = exe(if role == Role::Receiver { "kfm-receive" } else { "kfm-send" });
            let with = |extra: &[&str]| { let mut a = args.clone(); a.extend(extra.iter().map(|e| e.to_string())); a };
            if role == Role::WindowsSource {
                if p.os != "windows" { return Err("the windows-source role applies on Windows only".into()); }
                o.push("EXPERIMENTAL: the real step captures EVERY keyboard on this PC (a laptop's too) while forwarding, and has never run on real hardware. Recovery: press the hotkey again, move the mouse back, or close the console window with the mouse.".into());
                let km = p.ws.keymap_path().to_string_lossy().into_owned();
                o.push(format!("\nStep 1: dry run (no hook, no key is suppressed)\n  {}{}\n  rollback: Ctrl+C", sh.env_prefix("KFM_KEYMAP", &km), sh.command(&bin, &args)));
                o.push(format!("\nStep 2: real run, only after you approve\n  {}{}\n  rollback: press the hotkey, or close the console window; nothing is installed", sh.env_prefix("KFM_KEYMAP", &km), sh.command(&bin, &with(&["--experimental-capture-all-keyboards"]))));
                o.push("\nOnly the `mac` remaps of the keymap apply to this role (the receiver is a Mac).".into());
            } else {
                if p.os == "macos" { o.push("macOS receiver: real injection (CGEventPost) has never run on hardware; it needs the Accessibility permission for the app that starts it (the terminal when run by hand). kfm-config will not change permissions; grant it yourself in System Settings > Privacy & Security > Accessibility.".into()); }
                else { o.push("Windows may ask whether to let kfm-receive through the firewall the first time; that choice is yours (a private network is enough). kfm-config changes no firewall rule.".into()); }
                o.push(format!("\nStep 1: dry run (counts what it would inject, types nothing; stops after 30 s)\n  {}\n  rollback: Ctrl+C", sh.command(&bin, &with(&["--seconds", "30"]))));
                o.push(format!("\nStep 2: real injection, only after you approve\n  {}\n  rollback: Ctrl+C (held keys are released); nothing is installed", sh.command(&bin, &with(&["--enable-inject"]))));
            }
        }
    }
    Ok(o.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    /// A fresh folder whose name contains a space.
    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kfm settings {tag} {} {}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn ws(tag: &str) -> Workspace { let d = tmp(tag); Workspace { dir: d.join("local"), installed: Some(d.join("installed")) } }
    fn pair(k: &str, v: &str) -> Vec<(String, String)> { vec![(k.into(), v.into())] }
    fn set(w: &Workspace, k: &str, v: &str) -> Result<Option<PathBuf>, String> { Plan::set_values(w, &pair(k, v), None)?.commit(w) }
    fn complete(w: &Workspace, role: Role) {
        init(w, role).unwrap();
        for (k, v) in [("KFM_PEER", "192.0.2.10:47501"), ("KFM_BIND", "192.0.2.20:47502")] { set(w, k, v).unwrap(); }
    }

    #[test]
    fn parses_the_example_and_the_installer_forms() {
        let s = Settings::parse_with_home(include_str!("../examples/config.example.sh"), Some("/home/u")).unwrap();
        assert_eq!(s.get("KFM_KEY"), "/home/u/kfm-psk.key");
        assert_eq!(s.get("KFM_PEER"), "192.0.2.10:47501");
        assert_eq!(s.get("KFM_LINK_TIMEOUT"), "");
        let installed = "KFM_PEER=192.0.2.10:47501\nKFM_BIND=192.0.2.20:47502\nKFM_FLOW_EDGE=right\nKFM_FLOW_DISPLAY=1,2\nKFM_LINK_TIMEOUT=\nKFM_KEYBOARD_VENDOR_ID=\nKFM_KEYBOARD_PRODUCT_ID=\nKFM_TARGET_OS=windows\n";
        assert_eq!(Settings::parse(installed).unwrap().get("KFM_FLOW_DISPLAY"), "1,2");
        let q = Settings::parse_with_home("KFM_KEY='/a b/k'  # note\nKFM_KEYMAP=\"/c d/m.json\"\n", None).unwrap();
        assert_eq!((q.get("KFM_KEY"), q.get("KFM_KEYMAP")), ("/a b/k", "/c d/m.json"));
    }

    #[test]
    fn parser_fails_closed() {
        for bad in ["KFM_NOPE=1", "export KFM_KEY=/a", "KFM_KEY=$(whoami)", "KFM_KEY=/a b", "KFM_KEY=\"$HOME/k\"", "KFM_KEY='a", "KFM_KEY=/a;rm", "KFM_ROLE=receiver\nKFM_ROLE=receiver", "just text"] {
            assert!(Settings::parse_with_home(bad, Some("/h")).is_err(), "{bad}");
        }
    }

    #[test]
    fn written_file_is_sourceable_by_sh_and_round_trips() {
        let mut s = Settings::template(Role::MacSource, Path::new("/tmp/a b/local"));
        s.set("KFM_KEY", "/tmp/with space/$HOME `x` \"q\" ;&|<>.key");
        s.set("KFM_PEER", "192.0.2.10:47501");
        let text = s.render().unwrap();
        let back = Settings::parse_with_home(&text, None).unwrap();
        for k in KEYS { assert_eq!(back.get(k), s.get(k), "{k}"); }
        #[cfg(unix)]
        {
            let d = tmp("source");
            let f = d.join("config.sh");
            std::fs::write(&f, &text).unwrap();
            let out = std::process::Command::new("sh").arg("-c").arg(". \"$1\"; printf %s \"$KFM_KEY|$KFM_PEER|$KFM_FLOW_EDGE\"").arg("sh").arg(&f).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), "/tmp/with space/$HOME `x` \"q\" ;&|<>.key|192.0.2.10:47501|right");
        }
    }

    #[test]
    fn unquotable_values_are_refused() {
        for v in ["it's", "line\nbreak", "tab\there", "nul\0"] { assert!(check_quotable("KFM_KEY", v).is_err(), "{v:?}"); }
        let w = ws("quote");
        init(&w, Role::Receiver).unwrap();
        assert!(set(&w, "KFM_KEY", "/tmp/it's.key").is_err());
    }

    #[test]
    fn per_key_validation() {
        for (k, v) in [("KFM_ROLE", "mac-source"), ("KFM_PEER", "192.0.2.10:47501"), ("KFM_PEER", "[::1]:47501"), ("KFM_FLOW_EDGE", "top"), ("KFM_FLOW_DISPLAY", "all"), ("KFM_FLOW_DISPLAY", "1,2"),
                       ("KFM_LINK_TIMEOUT", "300"), ("KFM_LINK_TIMEOUT", ""), ("KFM_TARGET_OS", "mac"), ("KFM_KEYBOARD_VENDOR_ID", "0x1234"), ("KFM_KEY", "/k"), ("KFM_KEY", "C:\\k\\psk.key")] {
            check_value(k, v).unwrap_or_else(|e| panic!("{k}={v}: {e}"));
        }
        for (k, v) in [("KFM_ROLE", "boss"), ("KFM_PEER", "192.0.2.10"), ("KFM_PEER", "host:1"), ("KFM_PEER", "192.0.2.10:0"), ("KFM_FLOW_EDGE", "up"), ("KFM_FLOW_DISPLAY", "0"), ("KFM_FLOW_DISPLAY", "1,,2"),
                       ("KFM_FLOW_DISPLAY", "a"), ("KFM_LINK_TIMEOUT", "50"), ("KFM_LINK_TIMEOUT", "5000"), ("KFM_TARGET_OS", "linux"), ("KFM_KEYBOARD_VENDOR_ID", "0x10000"), ("KFM_KEYBOARD_PRODUCT_ID", "zz"),
                       ("KFM_KEY", "relative/k"), ("KFM_NOPE", "x")] {
            assert!(check_value(k, v).is_err(), "{k}={v}");
        }
    }

    #[test]
    fn incomplete_is_allowed_while_editing_but_not_to_validate() {
        let w = ws("incomplete");
        init(&w, Role::MacSource).unwrap();
        let before = std::fs::read_to_string(w.config_path()).unwrap();
        assert!(set(&w, "KFM_FLOW_EDGE", "left").is_ok(), "peer/bind are still empty");
        assert_ne!(std::fs::read_to_string(w.config_path()).unwrap(), before);
        let r = validate(&w, None).unwrap();
        assert!(!r.complete() && r.writable() && r.missing.iter().any(|m| m.contains("KFM_PEER")));
        assert!(!w.validated_path().exists(), "no fingerprint for an incomplete config");
        // the id pair: one alone is malformed, both together at once is fine
        assert!(Plan::set_values(&w, &pair("KFM_KEYBOARD_VENDOR_ID", "0x1234"), None).unwrap().commit(&w).is_err());
        let both = vec![("KFM_KEYBOARD_VENDOR_ID".to_string(), "0x1234".to_string()), ("KFM_KEYBOARD_PRODUCT_ID".to_string(), "0x5678".to_string())];
        Plan::set_values(&w, &both, None).unwrap().commit(&w).unwrap();
    }

    #[test]
    fn init_refuses_to_overwrite() {
        let w = ws("init");
        init(&w, Role::Receiver).unwrap();
        let text = std::fs::read_to_string(w.config_path()).unwrap();
        assert!(init(&w, Role::MacSource).is_err());
        assert_eq!(std::fs::read_to_string(w.config_path()).unwrap(), text);
        assert_eq!(std::fs::read_to_string(w.keymap_path()).unwrap(), "{}\n");
    }

    #[test]
    fn dry_run_preview_and_invalid_changes_write_nothing() {
        let w = ws("preview");
        complete(&w, Role::MacSource);
        let before = std::fs::read_to_string(w.config_path()).unwrap();
        let had = list_backups(&w).len();
        let plan = Plan::set_values(&w, &pair("KFM_FLOW_EDGE", "top"), None).unwrap();
        let p = plan.preview(false);
        assert!(p.contains("- KFM_FLOW_EDGE = right") && p.contains("+ KFM_FLOW_EDGE = top") && p.contains("valid: no problems"), "{p}");
        assert_eq!(std::fs::read_to_string(w.config_path()).unwrap(), before, "preview writes nothing");
        // invalid value: rejected before a plan exists
        assert!(Plan::set_values(&w, &pair("KFM_FLOW_EDGE", "up"), None).is_err());
        // invalid whole config: keymap broken by hand -> a config edit is refused and nothing changes
        std::fs::write(w.keymap_path(), "not json").unwrap();
        let bad = Plan::set_values(&w, &pair("KFM_FLOW_EDGE", "top"), None).unwrap();
        assert!(!bad.report.writable() && bad.commit(&w).is_err());
        assert_eq!(std::fs::read_to_string(w.config_path()).unwrap(), before);
        assert_eq!(list_backups(&w).len(), had, "a refused change makes no backup");
    }

    #[test]
    fn keymap_edits_use_remap_parse_for_every_rule() {
        let map = |t, f: &str, to: &str| KeyOp::Map { target: t, from: f.into(), to: to.into() };
        let w = TargetOs::Windows;
        let t1 = apply_key_op("{}", &map(w, "caps_lock", "left_control")).unwrap_err();
        assert!(t1.contains("would both be") && t1.contains("swap"), "{t1}");
        let t = apply_key_op("{}", &KeyOp::Drop { target: w, from: "left_control".into() }).unwrap();
        let t = apply_key_op(&t, &map(w, "caps_lock", "left_control")).unwrap();
        assert!(remap::parse(&t).unwrap().windows.len() == 2);
        let t = apply_key_op(&t, &map(w, "caps_lock", "escape")).unwrap_err();
        assert!(t.contains("would both be"), "escape is its own physical key: {t}");
        assert!(apply_key_op("{}", &map(w, "nope", "a")).unwrap_err().contains("not a forwarded key"));
        assert!(apply_key_op("{}", &map(w, "f13", "a")).is_err(), "the hotkey is not a forwardable source");
        assert!(apply_key_op("{}", &KeyOp::Hotkey("escape".into())).is_err());
        assert!(apply_key_op("{}", &KeyOp::Hotkey("f21".into())).is_err());
        let h = apply_key_op("{}", &KeyOp::Hotkey("F15".into())).unwrap();
        assert_eq!(remap::parse(&h).unwrap().hotkey, "f15");
        assert!(apply_key_op("{}", &KeyOp::Unmap { target: w, from: "a".into() }).is_err());
        assert!(apply_key_op("{}", &map(w, "a", "a")).is_err());
        // JIS keys and the left/right modifiers are separate names
        let jis = apply_key_op("{}", &KeyOp::Drop { target: TargetOs::Mac, from: "lang1".into() }).unwrap();
        assert!(remap::parse(&jis).is_ok() && jis.contains("lang1"));
        // swap, replace, unmap, reset
        let s = apply_key_op("{}", &map(TargetOs::Mac, "caps_lock", "left_control")).unwrap_err();
        assert!(s.contains("would both be") && s.contains("swap"));
        let s = apply_key_op("{}", &KeyOp::Swap { target: TargetOs::Mac, a: "caps_lock".into(), b: "left_control".into() }).unwrap();
        assert_eq!(remap::parse(&s).unwrap().mac.len(), 2, "a swap");
        let s = apply_key_op(&s, &KeyOp::ResetTarget(TargetOs::Mac)).unwrap();
        assert_eq!(s, "{}\n");
        let both = apply_key_op(&apply_key_op("{}", &KeyOp::Hotkey("f16".into())).unwrap(), &KeyOp::Drop { target: w, from: "insert".into() }).unwrap();
        assert_eq!(apply_key_op(&both, &KeyOp::ResetAll).unwrap(), "{}\n");
        assert_eq!(apply_key_op(&both, &KeyOp::Unmap { target: w, from: "insert".into() }).unwrap(), "{\n  \"hotkey\": \"f16\"\n}\n");
    }

    #[test]
    fn key_edit_writes_with_backup_and_restore_round_trips() {
        let w = ws("restore");
        complete(&w, Role::MacSource);
        let plan = Plan::key_edit(&w, &KeyOp::Hotkey("f15".into()), None).unwrap();
        assert!(plan.preview(false).contains("+ switch hotkey: f15") && plan.preview(false).contains("Karabiner rule must be applied again"));
        let had = list_backups(&w).len();
        let backup = plan.commit(&w).unwrap().expect("backup made");
        assert_eq!(list_backups(&w).len(), had + 1);
        assert!(backup.join("keymap.json").exists() && backup.join("config.sh").exists());
        assert!(std::fs::read_to_string(w.keymap_path()).unwrap().contains("f15"));
        let stamp = list_backups(&w).pop().unwrap();
        assert_eq!(std::fs::read_to_string(backup.join("keymap.json")).unwrap(), "{}\n", "the backup holds the previous keymap");
        let (undo_backup, done) = restore(&w, &stamp).unwrap();
        assert_eq!(done, vec!["config.sh", "keymap.json"]);
        assert_eq!(std::fs::read_to_string(w.keymap_path()).unwrap(), "{}\n");
        assert!(undo_backup.unwrap().join("keymap.json").exists(), "restore backs up the current files first");
        assert_eq!(list_backups(&w).len(), had + 2);
        assert!(restore(&w, "../x").is_err() && restore(&w, "19990101-000000").is_err());
    }

    #[test]
    fn utc_stamp_is_a_valid_folder_name() {
        assert_eq!(utc_stamp(0), "19700101-000000");
        assert_eq!(utc_stamp(1_700_000_000), "20231114-221320");
        assert_eq!(utc_stamp(1_709_164_800 + 86399), "20240229-235959");
        assert_eq!(utc_stamp(1_709_164_800 + 86400), "20240301-000000");
    }

    #[test]
    fn show_masks_addresses_and_paths_unless_revealed() {
        let w = ws("show");
        complete(&w, Role::MacSource);
        let s = Settings::parse(&std::fs::read_to_string(w.config_path()).unwrap()).unwrap();
        let masked = describe_settings(&s, false, true) + &settings_lines(&s, false).join("\n");
        for secret in ["192.0.2.10", "192.0.2.20", "kfm-psk.key", &*w.dir.to_string_lossy()] { assert!(!masked.contains(secret), "{secret} leaked:\n{masked}"); }
        assert!(masked.contains("192.x.x.x:47501"));
        let shown = describe_settings(&s, true, true);
        assert!(shown.contains("192.0.2.10:47501") && shown.contains("kfm-psk.key"));
        assert_eq!(mask_addr("[2001:db8::1]:5"), "[x:x:...]:5");
        let k = remap::parse(r#"{"hotkey":"f15","remap":{"windows":[["caps_lock","left_control"],["left_control","caps_lock"],["right_option","drop"]]}}"#).unwrap();
        let t = keymap_lines(&k).join("\n");
        assert!(t.contains("switch hotkey: f15") && t.contains("caps_lock sends left_control") && t.contains("right_option is dropped") && t.contains("target mac: no remaps"));
    }

    #[test]
    fn status_goes_edited_validated_applied_with_a_fake_installed_dir() {
        let w = ws("status");
        complete(&w, Role::MacSource);
        let st = |w: &Workspace, r| status(w, r).unwrap();
        assert_eq!(st(&w, Running::Unknown)[0].state, State::Edited);
        assert!(validate(&w, None).unwrap().complete());
        assert_eq!(st(&w, Running::Unknown)[0].state, State::Validated);
        assert_eq!(st(&w, Running::Unknown)[1].state, State::Applied, "no installed keymap equals the default one");
        set(&w, "KFM_FLOW_EDGE", "top").unwrap();
        assert_eq!(st(&w, Running::Unknown)[0].state, State::Edited, "a change after validate");
        assert!(validate(&w, None).unwrap().complete());
        // the installer's copy: unquoted, only its 8 keys
        let inst = w.installed.clone().unwrap();
        std::fs::create_dir_all(&inst).unwrap();
        std::fs::write(inst.join("config.sh"), "KFM_PEER=192.0.2.10:47501\nKFM_BIND=192.0.2.20:47502\nKFM_FLOW_EDGE=right\nKFM_FLOW_DISPLAY=all\nKFM_LINK_TIMEOUT=\nKFM_KEYBOARD_VENDOR_ID=\nKFM_KEYBOARD_PRODUCT_ID=\nKFM_TARGET_OS=windows\n").unwrap();
        assert_eq!(st(&w, Running::Unknown)[0].state, State::Validated, "edge differs: not applied yet");
        std::fs::write(inst.join("config.sh"), "KFM_PEER=192.0.2.10:47501\nKFM_BIND=192.0.2.20:47502\nKFM_FLOW_EDGE=top\nKFM_FLOW_DISPLAY=\nKFM_LINK_TIMEOUT=\nKFM_KEYBOARD_VENDOR_ID=\nKFM_KEYBOARD_PRODUCT_ID=\nKFM_TARGET_OS=windows\n").unwrap();
        let s = st(&w, Running::Unknown);
        assert_eq!(s[0].state, State::Applied);
        assert!(s[0].note.contains("restart to be sure"));
        assert!(st(&w, Running::NotRunning)[0].note.contains("not running"));
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        assert!(st(&w, Running::Since(long_ago))[0].note.contains("RESTART NEEDED"));
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        assert!(!st(&w, Running::Since(later))[0].note.contains("RESTART NEEDED"));
        // a keymap change is edited, then validated, then applied once copied
        Plan::key_edit(&w, &KeyOp::Hotkey("f15".into()), None).unwrap().commit(&w).unwrap();
        assert_eq!(st(&w, Running::Unknown)[1].state, State::Edited);
        validate(&w, None).unwrap();
        assert_eq!(st(&w, Running::Unknown)[1].state, State::Validated);
        std::fs::copy(w.keymap_path(), inst.join("keymap.json")).unwrap();
        assert_eq!(st(&w, Running::Unknown)[1].state, State::Applied);
    }

    #[test]
    fn rule_state_and_etime() {
        let pf = |hot: &str, dev: &str| format!("hotkey: configured f13, installed rule {hot}\ndevice: configured 0x320f:0x5055, installed rule {dev}\ncomplex rules: 2 (ours at Some(0))");
        assert_eq!(rule_state(&pf("f13", "0x320f:0x5055")), RuleState::Current);
        // same hotkey, only the keyboard id differs (or cannot be read): the old rule still captures the old keyboard
        assert!(matches!(rule_state(&pf("f13", "0x1234:0x5678")), RuleState::Differs { reason } if reason.contains("keyboard") && reason.contains("0x1234")));
        assert!(matches!(rule_state(&pf("f13", "?")), RuleState::Differs { .. }));
        assert!(matches!(rule_state(&pf("f15", "0x320f:0x5055")), RuleState::Differs { reason } if reason.contains("hotkey") && !reason.contains("keyboard")));
        assert!(matches!(rule_state(&pf("f15", "0x1:0x2")), RuleState::Differs { reason } if reason.contains("hotkey") && reason.contains("keyboard")));
        // the config names a keyboard but the rule still has the default one (and the reverse is the case above)
        assert!(matches!(rule_state("hotkey: configured f13, installed rule f13\ndevice: configured 0x1234:0x5678, installed rule 0x320f:0x5055\n"), RuleState::Differs { .. }));
        // the device line is missing or malformed: never current
        assert_eq!(rule_state("hotkey: configured f13, installed rule f13\ncomplex rules: 2 (ours at Some(0))"), RuleState::Unknown);
        assert_eq!(rule_state("hotkey: configured f13, installed rule f13\ndevice: garbage\n"), RuleState::Unknown);
        assert_eq!(rule_state("complex rules: 1 (ours at None)"), RuleState::NotInstalled);
        assert_eq!(rule_state("nothing"), RuleState::Unknown);
        assert_eq!(parse_etime(" 05:07"), Some(307));
        assert_eq!(parse_etime("1:02:03"), Some(3723));
        assert_eq!(parse_etime("2-00:00:01"), Some(172801));
        assert_eq!(parse_etime("x"), None);
    }

    #[test]
    fn layout_warns_but_does_not_fail() {
        let two = [Rect { x: 0.0, y: 0.0, w: 1000.0, h: 800.0 }, Rect { x: 1000.0, y: 0.0, w: 1000.0, h: 800.0 }];
        assert_eq!(parse_display_list("display 1 x=0 y=0 w=1000 h=800 primary=true\nnoise\ndisplay 2 x=1000 y=0 w=1000 h=800 primary=false\n").len(), 2);
        let mut s = Settings::template(Role::MacSource, Path::new("/d"));
        s.set("KFM_FLOW_DISPLAY", "all");
        assert!(layout_warnings(&s, &two).is_empty(), "right edge of display 2 is outer");
        s.set("KFM_FLOW_DISPLAY", "1");
        let w = layout_warnings(&s, &two);
        assert!(w.len() == 1 && w[0].contains("not an outer edge"), "{w:?}");
        s.set("KFM_FLOW_DISPLAY", "3");
        assert!(layout_warnings(&s, &two)[0].contains("not 1..2"));
        s.set("KFM_FLOW_DISPLAY", "1,2");
        s.set("KFM_FLOW_EDGE", "top");
        assert!(layout_warnings(&s, &two).is_empty());
        // through the whole check: warnings only
        s.set("KFM_FLOW_DISPLAY", "1");
        s.set("KFM_FLOW_EDGE", "right");
        let r = check_settings(&s, None, Some(&two));
        assert!(r.errors.is_empty() && r.warnings.iter().any(|w| w.contains("outer edge")));
    }

    #[test]
    fn run_args_quote_paths_with_spaces() {
        let mut s = Settings::template(Role::Receiver, Path::new("/home/me/my repo/local"));
        s.set("KFM_PEER", "192.0.2.20:47502");
        s.set("KFM_BIND", "192.0.2.10:47501");
        let a = run_args(&s, Role::Receiver).unwrap();
        assert_eq!(a[..4], ["--bind", "192.0.2.10:47501", "--peer", "192.0.2.20:47502"]);
        let key = Path::new("/home/me/my repo/local").join("kfm-psk.key").to_string_lossy().into_owned(); // separator depends on the OS running the test
        assert_eq!(Shell::Sh.join(&a), format!("--bind 192.0.2.10:47501 --peer 192.0.2.20:47502 --key-file '{key}' --flow-edge left --flow-display all"));
        assert!(Shell::PowerShell.join(&a).contains(&format!("--key-file '{key}'")));
        assert_eq!(Shell::PowerShell.quote("C:\\Users\\a b\\it's.key"), "'C:\\Users\\a b\\it''s.key'");
        assert_eq!(Shell::Sh.quote("it's"), "'it'\\''s'");
        assert_eq!(Shell::Sh.quote(""), "''");
        assert_eq!(Shell::PowerShell.command("C:\\x y\\kfm-receive.exe", &["--a".into()]), "& 'C:\\x y\\kfm-receive.exe' --a");
        assert!(run_args(&s, Role::WindowsSource).is_err(), "role mismatch");
        let mut w = Settings::template(Role::WindowsSource, Path::new("/d"));
        w.set("KFM_PEER", "192.0.2.20:47502");
        w.set("KFM_BIND", "192.0.2.10:47501");
        assert!(!run_args(&w, Role::WindowsSource).unwrap().iter().any(|a| a.starts_with("--flow")), "no edge, no flow arguments");
        w.set("KFM_FLOW_EDGE", "left");
        w.set("KFM_LINK_TIMEOUT", "400");
        assert_eq!(run_args(&w, Role::WindowsSource).unwrap()[6..], ["--link-timeout-ms", "400", "--flow-edge", "left", "--flow-display", "all"]);
        let incomplete = Settings::template(Role::Receiver, Path::new("/d"));
        assert!(run_args(&incomplete, Role::Receiver).is_err());
    }

    #[test]
    fn apply_plan_is_text_only_and_has_rollbacks() {
        let w = ws("plan");
        complete(&w, Role::MacSource);
        let s = Settings::parse(&std::fs::read_to_string(w.config_path()).unwrap()).unwrap();
        let root = Path::new("/r repo");
        let foreign = Jobs { ours_loaded: vec![], others: vec!["local.r75-mac".into()] };
        let mk = |jobs| PlanCtx { settings: &s, ws: &w, repo_root: root, shell: Shell::Sh, os: "macos", rule: RuleState::Differs { reason: "hotkey: the installed rule uses f13 but the keymap says f15".into() }, jobs, key_exists: false, validated: true };
        let stop = apply_plan(&mk(Some(&foreign))).unwrap_err();
        assert!(stop.contains("STOP") && stop.contains("local.r75-mac") && !stop.contains("Step 1") && !stop.contains("--confirm-backup-and-apply"), "{stop}");
        let jobs = Jobs { ours_loaded: vec!["local.kfm-mac".into()], others: vec![] };
        let t = apply_plan(&mk(Some(&jobs))).unwrap();
        for want in ["keygen", "--confirm-backup-and-apply", "rollback:", "uninstall.sh", "--apply", "install.sh'", "/r repo", "BEFORE the install step"] { assert!(t.contains(want), "{want}\n{t}"); }
        assert!(t.matches("rollback").count() >= 4);
        let mut r = Settings::template(Role::Receiver, Path::new("/d"));
        r.set("KFM_PEER", "192.0.2.20:47502");
        r.set("KFM_BIND", "192.0.2.10:47501");
        let ctx = PlanCtx { settings: &r, ws: &w, repo_root: root, shell: Shell::Sh, os: "windows", rule: RuleState::Unknown, jobs: None, key_exists: true, validated: true };
        let t = apply_plan(&ctx).unwrap();
        assert!(t.contains("--seconds 30") && t.contains("--enable-inject") && t.contains("firewall"), "{t}");
    }

    #[test]
    fn relative_key_path_is_resolved_and_unchanged_keys_are_previewed() {
        assert_eq!(resolve_path("./local/kfm-psk.key", Path::new("/a b/repo")), Path::new("/a b/repo").join("local").join("kfm-psk.key").to_string_lossy());
        assert_eq!(resolve_path("../k", Path::new("/a/b")), Path::new("/a").join("k").to_string_lossy());
        let w = ws("relative");
        complete(&w, Role::MacSource);
        let plan = Plan::set_values(&w, &[("KFM_KEY".into(), "./local/kfm-psk.key".into()), ("KFM_TARGET_OS".into(), "windows".into())], None).unwrap();
        assert!(plan.report.writable(), "{}", plan.report.summary());
        let p = plan.preview(false);
        let abs = resolve_path("./local/kfm-psk.key", &std::env::current_dir().unwrap());
        assert!(p.contains("relative path") && p.contains(&abs), "the resolved path is shown: {p}");
        assert!(p.contains("= KFM_TARGET_OS = windows (unchanged)"), "{p}");
        plan.commit(&w).unwrap();
        assert_eq!(Settings::parse(&std::fs::read_to_string(w.config_path()).unwrap()).unwrap().get("KFM_KEY"), abs);
    }

    #[test]
    fn doctor_reports_cause_next_and_undo_and_flags_existing_installs() {
        let w = ws("doctor");
        let facts = |jobs| Facts { os: "macos", arch: "aarch64", binaries: vec![("kfm-mac".into(), true), ("kfm-karabiner".into(), true), ("kfm-receive".into(), false)],
            karabiner_installed: Some(true), preflight: Some(Ok("hotkey: configured f13, installed rule f13\ncomplex rules: 1 (ours at Some(0))".into())), jobs, displays: None, repo_root: PathBuf::from("/r") };
        let c = doctor_checks(&w, &facts(None));
        assert!(c.iter().any(|c| c.level == Level::Problem && c.title.contains("not built") && !c.cause.is_empty() && !c.next.is_empty() && !c.undo.is_empty()));
        assert!(c.iter().any(|c| c.level == Level::Problem && c.title.contains("does not exist")));
        let early = doctor_checks(&w, &facts(Some(Jobs { ours_loaded: vec![], others: vec!["local.r75-mac".into()] })));
        assert!(early.iter().any(|c| c.level == Level::Problem && c.title.contains("existing installation")), "reported before any settings exist");
        complete(&w, Role::MacSource);
        let jobs = Jobs { ours_loaded: vec![], others: vec!["local.r75-guard".into()] };
        let c = doctor_checks(&w, &facts(Some(jobs)));
        assert!(c.iter().any(|c| c.level == Level::Problem && c.title.contains("existing installation") && c.title.contains("Do not overwrite")));
        assert!(c.iter().any(|c| c.level == Level::Problem && c.title.contains("key file does not exist")));
        assert!(!c.iter().any(|c| c.render().contains("192.0.2")), "doctor never prints the addresses");
    }
}
