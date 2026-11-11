//! Mode A: Karabiner-Elements selects Rainy75 and suppresses its keys; this
//! module is the bridge between Karabiner and the router (pure; no sockets,
//! no karabiner_cli calls — the adapter performs `Out` actions).
//!
//! Karabiner pipeline (official docs): hardware -> simple_modifications ->
//! complex_modifications (first matching manipulator wins) -> virtual keyboard.
//! So the rules below see keys *after* the Rainy75 simple modifications (the
//! existing Cmd<->Ctrl swap): a physical Cmd/Win key arrives as left_control.
//! For Windows, kfm-mac undoes that swap with `WindowsProfile::undo_swaps`
//! (read from karabiner.json), so Windows gets the physical key.
//!
//! - `kfm_route` = 1: every Rainy75 key matches a `device_if` + `variable_if`
//!   manipulator whose only output is `send_user_command` (nothing reaches the
//!   virtual keyboard, i.e. the Mac). Other keyboards never match.
//! - `kfm_route` = 0 (or unset): the rules are inactive and Karabiner's current
//!   behaviour is untouched.
//! - F13 on Rainy75: with route 1 it sets route 0 inside Karabiner itself (the
//!   escape works even if the bridge is dead) and tells the bridge; with route
//!   0 it only asks the bridge, which turns route 1 on after the router agrees
//!   (link confirmed). Switching *to* Windows therefore fails closed.

use serde_json::{Value, json};

use crate::keys::Usage;
use crate::router::{Dest, Effect, Reason, Router, Target, Local};
use crate::keys::WinKey;

/// The keyboard this was built and tested with (Rainy75).
pub const RAINY75_VENDOR_ID: u64 = 0x320F;
pub const RAINY75_PRODUCT_ID: u64 = 0x5055;
pub const TESTED_KEYBOARD: Keyboard = Keyboard { vendor_id: RAINY75_VENDOR_ID, product_id: RAINY75_PRODUCT_ID };
pub const KEYBOARD_VENDOR_ENV: &str = "KFM_KEYBOARD_VENDOR_ID";
pub const KEYBOARD_PRODUCT_ENV: &str = "KFM_KEYBOARD_PRODUCT_ID";

/// The keyboard whose keys are routed, as Karabiner-Elements identifies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keyboard { pub vendor_id: u64, pub product_id: u64 }

/// `KFM_KEYBOARD_VENDOR_ID` and `KFM_KEYBOARD_PRODUCT_ID` (decimal or 0x-hex,
/// both or neither); unset = the tested keyboard. The binaries call this at
/// start-up and refuse to run on an error.
pub fn keyboard_from_env() -> Result<Keyboard, String> {
    keyboard_from(std::env::var(KEYBOARD_VENDOR_ENV).ok().as_deref(), std::env::var(KEYBOARD_PRODUCT_ENV).ok().as_deref())
}

pub fn keyboard_from(vendor: Option<&str>, product: Option<&str>) -> Result<Keyboard, String> {
    let parse = |name: &str, v: &str| {
        let n = match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) { Some(h) => u64::from_str_radix(h, 16).ok(), None => v.parse().ok() };
        n.filter(|n| *n > 0 && *n <= 0xFFFF).ok_or_else(|| format!("{name} must be a USB id (1..=0xFFFF, decimal or 0x-hex), got {v:?}"))
    };
    match (vendor, product) {
        (None, None) => Ok(TESTED_KEYBOARD),
        (Some(v), Some(p)) => Ok(Keyboard { vendor_id: parse(KEYBOARD_VENDOR_ENV, v)?, product_id: parse(KEYBOARD_PRODUCT_ENV, p)? }),
        _ => Err(format!("set both {KEYBOARD_VENDOR_ENV} and {KEYBOARD_PRODUCT_ENV}, or neither")),
    }
}

/// The configured keyboard (see `keyboard_from_env`).
pub fn keyboard() -> Keyboard {
    static K: std::sync::OnceLock<Keyboard> = std::sync::OnceLock::new();
    *K.get_or_init(|| keyboard_from_env().expect("keyboard id is checked at start-up"))
}
pub const ROUTE_VAR: &str = "kfm_route";
const TAG: &str = "kfm";
const MAX_DATAGRAM: usize = 256;

/// Karabiner `key_code` -> HID usage. F13 is the switch key and is not here.
pub const KEY_CODES: &[(&str, u16)] = &[
    ("a", 0x04), ("b", 0x05), ("c", 0x06), ("d", 0x07), ("e", 0x08), ("f", 0x09), ("g", 0x0A),
    ("h", 0x0B), ("i", 0x0C), ("j", 0x0D), ("k", 0x0E), ("l", 0x0F), ("m", 0x10), ("n", 0x11),
    ("o", 0x12), ("p", 0x13), ("q", 0x14), ("r", 0x15), ("s", 0x16), ("t", 0x17), ("u", 0x18),
    ("v", 0x19), ("w", 0x1A), ("x", 0x1B), ("y", 0x1C), ("z", 0x1D),
    ("1", 0x1E), ("2", 0x1F), ("3", 0x20), ("4", 0x21), ("5", 0x22), ("6", 0x23), ("7", 0x24),
    ("8", 0x25), ("9", 0x26), ("0", 0x27),
    ("return_or_enter", 0x28), ("escape", 0x29), ("delete_or_backspace", 0x2A), ("tab", 0x2B),
    ("spacebar", 0x2C), ("hyphen", 0x2D), ("equal_sign", 0x2E), ("open_bracket", 0x2F),
    ("close_bracket", 0x30), ("backslash", 0x31), ("semicolon", 0x33), ("quote", 0x34),
    ("grave_accent_and_tilde", 0x35), ("comma", 0x36), ("period", 0x37), ("slash", 0x38),
    ("caps_lock", 0x39),
    ("f1", 0x3A), ("f2", 0x3B), ("f3", 0x3C), ("f4", 0x3D), ("f5", 0x3E), ("f6", 0x3F),
    ("f7", 0x40), ("f8", 0x41), ("f9", 0x42), ("f10", 0x43), ("f11", 0x44), ("f12", 0x45),
    ("insert", 0x49), ("home", 0x4A), ("page_up", 0x4B), ("delete_forward", 0x4C), ("end", 0x4D),
    ("page_down", 0x4E), ("right_arrow", 0x4F), ("left_arrow", 0x50), ("down_arrow", 0x51),
    ("up_arrow", 0x52), ("lang1", 0x90), ("lang2", 0x91),
    ("left_control", 0xE0), ("left_shift", 0xE1), ("left_option", 0xE2), ("left_command", 0xE3),
    ("right_control", 0xE4), ("right_shift", 0xE5), ("right_option", 0xE6), ("right_command", 0xE7),
];

pub fn usage_of(key_code: &str) -> Option<Usage> {
    KEY_CODES.iter().find(|(n, _)| *n == key_code).map(|(_, u)| Usage(*u))
}

/// For configuration messages only (never for typed keys).
pub fn name_of(usage: Usage) -> Option<&'static str> {
    KEY_CODES.iter().find(|(_, u)| *u == usage.0).map(|(n, _)| *n)
}

// ---- Karabiner rule generation ----

fn rainy75_only() -> Value {
    let k = keyboard();
    json!({"type": "device_if", "identifiers": [{"vendor_id": k.vendor_id, "product_id": k.product_id}]})
}

fn route(on: bool) -> Value {
    json!({"type": if on { "variable_if" } else { "variable_unless" }, "name": ROUTE_VAR, "value": 1})
}

fn command(payload: Value, endpoint: Option<&str>) -> Value {
    let mut c = json!({"payload": payload});
    if let Some(e) = endpoint { c["endpoint"] = json!(e); }
    json!({"send_user_command": c})
}

/// One complex_modifications rule. It must be placed first (top priority) so
/// that, while route = 1, no other rule handles Rainy75 keys first.
pub fn rule(endpoint: Option<&str>) -> Value {
    rule_with_hotkey(endpoint, &crate::remap::key_config().hotkey)
}

/// `rule` for an explicit hotkey (a validated `remap::KeyConfig::hotkey`). A
/// hotkey that is one of the forwarded keys is not forwarded.
pub fn rule_with_hotkey(endpoint: Option<&str>, hotkey: &str) -> Value {
    let mut manipulators = vec![
        json!({
            "type": "basic",
            "from": {"key_code": hotkey, "modifiers": {"optional": ["any"]}},
            "conditions": [rainy75_only(), route(true)],
            "to": [{"set_variable": {"name": ROUTE_VAR, "value": 0}}, command(json!({TAG: 1, "e": "escape"}), endpoint)],
        }),
        json!({
            "type": "basic",
            "from": {"key_code": hotkey, "modifiers": {"optional": ["any"]}},
            "conditions": [rainy75_only(), route(false)],
            "to": [command(json!({TAG: 1, "e": "toggle"}), endpoint)],
        }),
    ];
    for (name, _) in KEY_CODES.iter().filter(|(n, _)| *n != hotkey) {
        manipulators.push(json!({
            "type": "basic",
            "from": {"key_code": name, "modifiers": {"optional": ["any"]}},
            "conditions": [rainy75_only(), route(true)],
            "to": [command(json!({TAG: 1, "k": name, "d": 1}), endpoint)],
            "to_after_key_up": [command(json!({TAG: 1, "k": name, "d": 0}), endpoint)],
        }));
    }
    json!({"description": "keyboard-follows-mouse: route the keyboard to Windows (generated)", "manipulators": manipulators})
}

// ---- datagram parsing ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeIn { Key { usage: Usage, down: bool }, Escape, ToggleRequest }

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError { TooLarge, NotJson, NotOurs, UnknownKey, Malformed }

/// Strict: only the exact shapes `rule()` generates are accepted.
pub fn parse(datagram: &[u8]) -> Result<BridgeIn, ParseError> {
    if datagram.len() > MAX_DATAGRAM { return Err(ParseError::TooLarge); }
    let v: Value = serde_json::from_slice(datagram).map_err(|_| ParseError::NotJson)?;
    let obj = v.as_object().ok_or(ParseError::NotOurs)?;
    if obj.get(TAG) != Some(&json!(1)) { return Err(ParseError::NotOurs); }
    match (obj.get("k"), obj.get("d"), obj.get("e"), obj.len()) {
        (Some(k), Some(d), None, 3) => {
            let usage = k.as_str().and_then(usage_of).ok_or(ParseError::UnknownKey)?;
            let down = match d.as_u64() { Some(1) => true, Some(0) => false, _ => return Err(ParseError::Malformed) };
            Ok(BridgeIn::Key { usage, down })
        }
        (None, None, Some(e), 2) => match e.as_str() {
            Some("escape") => Ok(BridgeIn::Escape),
            Some("toggle") => Ok(BridgeIn::ToggleRequest),
            _ => Err(ParseError::Malformed),
        },
        _ => Err(ParseError::Malformed),
    }
}

// ---- bridge ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    Remote { key: WinKey, down: bool, repeat: bool },
    /// Run `karabiner_cli --set-variables '{"kfm_route": 0|1}'`.
    SetRoute { remote: bool },
    Dest { dest: Dest, reason: Reason },
    Refused,
    /// Karabiner had already swallowed a key the router meant for the Mac
    /// (cannot be re-delivered without posting events; counted and surfaced).
    MacKeyDropped,
}

/// Auto-repeat: Karabiner sends one command per press, so the bridge repeats
/// the newest held non-modifier key itself.
#[derive(Clone, Copy, Debug)]
pub struct Typematic { pub delay_ms: u64, pub interval_ms: u64 }

impl Default for Typematic {
    fn default() -> Self { Typematic { delay_ms: 500, interval_ms: 33 } }
}

pub struct Bridge {
    router: Router,
    karabiner_remote: bool,
    typematic: Typematic,
    repeating: Option<(Usage, u64)>,
    /// K3 (stage 7b): macOS Secure Input is on, so nothing may move to Windows.
    secure_input: bool,
    pub mac_keys_dropped: u64,
}

impl Bridge {
    pub fn new(router: Router, typematic: Typematic) -> Self {
        Bridge { router, karabiner_remote: false, typematic, repeating: None, secure_input: false, mac_keys_dropped: 0 }
    }

    pub fn dest(&self) -> Dest { self.router.effective() }
    pub fn karabiner_remote(&self) -> bool { self.karabiner_remote }
    pub fn held_drops(&self) -> u64 { self.router.held_drops }

    pub fn on_input(&mut self, ev: BridgeIn, now_ms: u64) -> Vec<Out> {
        let mut out = Vec::new();
        match ev {
            BridgeIn::Key { usage, down } => {
                let res = self.router.on_key(usage, down);
                if res.local == Local::Pass {
                    self.mac_keys_dropped += 1;
                    out.push(Out::MacKeyDropped);
                }
                let forwarded = res.effects.iter().any(|e| matches!(e, Effect::Remote { down: true, .. }));
                if down && forwarded && !usage.is_modifier() { self.repeating = Some((usage, now_ms + self.typematic.delay_ms)); }
                if !down && self.repeating.is_some_and(|(u, _)| u == usage) { self.repeating = None; }
                out.extend(res.effects.into_iter().map(convert));
            }
            BridgeIn::Escape => {
                // Karabiner already set route 0: keys held on Windows will not
                // be seen again, so release them now.
                self.karabiner_remote = false;
                self.repeating = None;
                out.extend(self.router.release_remote().into_iter().map(convert));
                out.extend(self.router.select(Target::Mac, Reason::Hotkey).into_iter().map(convert));
            }
            BridgeIn::ToggleRequest if self.secure_input => {}
            BridgeIn::ToggleRequest => {
                out.extend(self.router.select(Target::Windows, Reason::Hotkey).into_iter().map(convert));
            }
        }
        self.sync(&mut out);
        out
    }

    pub fn on_link(&mut self, up: bool) -> Vec<Out> {
        let mut out: Vec<Out> = self.router.on_link(up).into_iter().map(convert).collect();
        if !up { self.repeating = None; }
        self.sync(&mut out);
        out
    }

    /// Stage 7b: the Flow follower decided where the mouse is. Applied like the
    /// hotkey (Karabiner's route follows), so a later Fn+F12 overrides it until the
    /// follower's next decision; refused for Windows while the link is down or
    /// Secure Input is on.
    pub fn on_flow(&mut self, target: Target) -> Vec<Out> { self.follow(target, Reason::Flow) }

    /// Rule E: the MX4 arrived on / left the Windows PC (Easy-Switch or Flow).
    /// Same gate as `on_flow`; selecting where it already is changes nothing,
    /// so rule C and rule E agreeing never switch twice.
    pub fn on_easy_switch(&mut self, target: Target) -> Vec<Out> { self.follow(target, Reason::EasySwitch) }

    fn follow(&mut self, target: Target, reason: Reason) -> Vec<Out> {
        if target == Target::Windows && self.secure_input { return vec![]; }
        let mut out: Vec<Out> = self.router.select(target, reason).into_iter().map(convert).collect();
        self.sync(&mut out);
        out
    }

    /// Resident mode: the link stayed down too long while Windows was selected,
    /// so give the keyboard back to the Mac (route 0) instead of holding forever.
    /// Keys still held on Windows were released when the link went down.
    pub fn fallback_to_mac(&mut self) -> Vec<Out> {
        let mut out: Vec<Out> = self.router.select(Target::Mac, Reason::LinkDown).into_iter().map(convert).collect();
        self.sync(&mut out);
        out
    }

    /// K3 (stage 7b): while macOS Secure Input is on (a password field), keys go
    /// to the Mac: Windows is left (its held keys released) and not re-entered
    /// when it ends; the next Flow decision or Fn+F12 moves again.
    pub fn on_secure_input(&mut self, active: bool) -> Vec<Out> {
        self.secure_input = active;
        let mut out: Vec<Out> = self.router.on_secure_input(active).into_iter().map(convert).collect();
        if active { out.extend(self.router.select(Target::Mac, Reason::SecureInput).into_iter().map(convert)); }
        self.sync(&mut out);
        out
    }

    /// Call every few ms; emits auto-repeat.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Out> {
        let Some((usage, due)) = self.repeating else { return vec![] };
        if now_ms < due { return vec![]; }
        self.repeating = Some((usage, now_ms + self.typematic.interval_ms));
        self.router.on_key(usage, true).effects.into_iter().map(convert).collect()
    }

    /// Karabiner's route follows the router: suppress on the Mac exactly while
    /// keys must not reach it (Windows or Held).
    fn sync(&mut self, out: &mut Vec<Out>) {
        let want = self.router.effective() != Dest::Mac;
        if want == self.karabiner_remote { return; }
        if !want {
            self.repeating = None;
            out.extend(self.router.release_remote().into_iter().map(convert));
        }
        self.karabiner_remote = want;
        out.push(Out::SetRoute { remote: want });
    }
}

/// K3 (stage 7b): one Secure Input poll. `holder` and `locked` are `None` when
/// the session could not be read.
#[derive(Clone, Debug, PartialEq)]
pub struct SecureProbe { pub enabled: bool, pub holder: Option<SecureHolder>, pub locked: Option<bool> }

#[derive(Clone, Debug, PartialEq)]
pub struct SecureHolder { pub pid: i32, pub name: String, pub loginwindow: bool }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SecureVerdict {
    Off,
    /// Keys stay on the Mac.
    Hold,
    /// loginwindow kept it after an unlock (a known macOS fault): ignored, since
    /// no loginwindow field can take keys while the screen is unlocked.
    StuckLoginwindow,
}

pub fn secure_verdict(p: &SecureProbe) -> SecureVerdict {
    if !p.enabled { return SecureVerdict::Off; }
    match (&p.holder, p.locked) {
        (Some(h), Some(false)) if h.loginwindow => SecureVerdict::StuckLoginwindow,
        _ => SecureVerdict::Hold,
    }
}

fn convert(e: Effect) -> Out {
    match e {
        Effect::Remote { key, down, repeat } => Out::Remote { key, down, repeat },
        Effect::DestChanged { dest, reason } => Out::Dest { dest, reason },
        Effect::RefusedLinkDown { .. } => Out::Refused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::WindowsProfile;
    use crate::keys::{hid, win_key_for_usage};
    use crate::router::Hotkey;

    fn w(u: Usage) -> WinKey { win_key_for_usage(u).unwrap() }
    fn key(name: &str, down: bool) -> BridgeIn { BridgeIn::Key { usage: usage_of(name).unwrap(), down } }
    fn bridge() -> Bridge { Bridge::new(Router::new(WindowsProfile::identity(), Hotkey::default()), Typematic::default()) }
    fn remote(u: Usage, down: bool) -> Out { Out::Remote { key: w(u), down, repeat: false } }

    // A deliberately small model of Karabiner's matching (first manipulator
    // whose from.key_code, device_if and variable conditions match). It checks
    // what the generated JSON says; it is not evidence of Karabiner's behaviour.
    enum Sim { Mac, Command(Value) }
    fn simulate(rule: &Value, vid: u64, pid: u64, key_code: &str, down: bool, route_var: Option<u64>) -> Sim {
        for m in rule["manipulators"].as_array().unwrap() {
            if m["from"]["key_code"] != key_code { continue; }
            let ok = m["conditions"].as_array().unwrap().iter().all(|c| match c["type"].as_str().unwrap() {
                "device_if" => c["identifiers"].as_array().unwrap().iter().any(|i| i["vendor_id"] == vid && i["product_id"] == pid),
                "variable_if" => route_var == c["value"].as_u64(),
                "variable_unless" => route_var != c["value"].as_u64(),
                other => panic!("unexpected condition {other}"),
            });
            if !ok { continue; }
            let list = if down { &m["to"] } else { &m["to_after_key_up"] };
            let to = list.as_array().unwrap();
            assert!(to.iter().all(|t| t.get("key_code").is_none()), "{key_code}: a remote manipulator must not emit keys to the Mac");
            return to.iter().find_map(|t| t.get("send_user_command")).map(|c| Sim::Command(c["payload"].clone())).unwrap_or(Sim::Mac);
        }
        Sim::Mac
    }

    #[test]
    fn a_configured_hotkey_switches_and_is_not_forwarded() {
        let r = rule_with_hotkey(None, "f15");
        assert!(matches!(simulate(&r, RAINY75_VENDOR_ID, RAINY75_PRODUCT_ID, "f15", true, Some(1)), Sim::Command(_)));
        let froms: Vec<&str> = r["manipulators"].as_array().unwrap().iter().filter_map(|m| m["from"]["key_code"].as_str()).collect();
        assert_eq!(froms.iter().filter(|f| **f == "f15").count(), 2, "only the two switch manipulators");
        assert!(!froms.contains(&"f13"), "the default hotkey is gone");
        assert_eq!(froms.len(), KEY_CODES.len() + 2);
        // The default rule still has F13 and every forwarded key.
        let d = rule_with_hotkey(None, "f13");
        assert_eq!(d["manipulators"].as_array().unwrap().len(), KEY_CODES.len() + 2);
    }

    #[test]
    fn keyboard_id_from_env_values() {
        assert_eq!(keyboard_from(None, None), Ok(TESTED_KEYBOARD));
        assert_eq!(keyboard_from(Some("0x046D"), Some("49152")), Ok(Keyboard { vendor_id: 0x046D, product_id: 49152 }));
        assert!(keyboard_from(Some("0x046D"), None).is_err(), "both or neither");
        for bad in ["", "0", "0x10000", "-1", "abc", "0x"] { assert!(keyboard_from(Some(bad), Some("1")).is_err(), "{bad:?}"); }
    }

    #[test]
    fn every_rule_is_limited_to_rainy75_and_the_route_variable() {
        let r = rule(None);
        let ms = r["manipulators"].as_array().unwrap();
        assert_eq!(ms.len(), KEY_CODES.len() + 2);
        for m in ms {
            let conds = m["conditions"].as_array().unwrap();
            assert!(conds.contains(&rainy75_only()), "missing device_if: {m}");
            assert!(conds.iter().any(|c| c["name"] == ROUTE_VAR), "missing route condition: {m}");
            assert_eq!(m["from"]["modifiers"]["optional"], json!(["any"]));
        }
        // The rule text round-trips through serde (valid JSON for karabiner.json).
        let _: Value = serde_json::from_str(&r.to_string()).unwrap();
    }

    #[test]
    fn generated_payloads_parse_back() {
        let r = rule(Some("/tmp/x.sock"));
        for m in r["manipulators"].as_array().unwrap() {
            for list in ["to", "to_after_key_up"] {
                for t in m[list].as_array().into_iter().flatten() {
                    if let Some(c) = t.get("send_user_command") {
                        assert_eq!(c["endpoint"], "/tmp/x.sock");
                        assert!(parse(c["payload"].to_string().as_bytes()).is_ok(), "{c}");
                    }
                }
            }
        }
    }

    #[test]
    fn model_two_keyboards_only_rainy75_is_routed() {
        let r = rule(None);
        let other = (7805, 12570); // the second keyboard configured in karabiner.json
        for (name, _) in KEY_CODES {
            // Route 1: Rainy75 keys become commands (suppressed on the Mac), down and up.
            for down in [true, false] {
                match simulate(&r, RAINY75_VENDOR_ID, RAINY75_PRODUCT_ID, name, down, Some(1)) {
                    Sim::Command(p) => assert_eq!(parse(p.to_string().as_bytes()).unwrap(), key(name, down)),
                    Sim::Mac => panic!("{name} leaked to the Mac"),
                }
                assert!(matches!(simulate(&r, other.0, other.1, name, down, Some(1)), Sim::Mac), "{name}: other keyboard captured");
                // Route 0 / unset: nothing is touched.
                assert!(matches!(simulate(&r, RAINY75_VENDOR_ID, RAINY75_PRODUCT_ID, name, down, Some(0)), Sim::Mac));
                assert!(matches!(simulate(&r, RAINY75_VENDOR_ID, RAINY75_PRODUCT_ID, name, down, None), Sim::Mac));
            }
        }
    }

    #[test]
    fn model_f13_escape_is_karabiner_native() {
        let r = rule(None);
        let m = &r["manipulators"][0];
        assert_eq!(m["to"][0], json!({"set_variable": {"name": ROUTE_VAR, "value": 0}}));
        match simulate(&r, RAINY75_VENDOR_ID, RAINY75_PRODUCT_ID, "f13", true, Some(1)) {
            Sim::Command(p) => assert_eq!(parse(p.to_string().as_bytes()), Ok(BridgeIn::Escape)),
            Sim::Mac => panic!(),
        }
        match simulate(&r, RAINY75_VENDOR_ID, RAINY75_PRODUCT_ID, "f13", true, None) {
            Sim::Command(p) => assert_eq!(parse(p.to_string().as_bytes()), Ok(BridgeIn::ToggleRequest)),
            Sim::Mac => panic!(),
        }
    }

    #[test]
    fn parser_is_strict() {
        assert_eq!(parse(br#"{"kfm":1,"k":"a","d":1}"#), Ok(key("a", true)));
        assert_eq!(parse(br#"{"kfm":1,"k":"left_command","d":0}"#), Ok(key("left_command", false)));
        assert_eq!(parse(br#"{"kfm":1,"e":"escape"}"#), Ok(BridgeIn::Escape));
        assert_eq!(parse(br#"{"kfm":1,"k":"f13","d":1}"#), Err(ParseError::UnknownKey));
        assert_eq!(parse(br#"{"kfm":1,"k":"a","d":2}"#), Err(ParseError::Malformed));
        assert_eq!(parse(br#"{"kfm":1,"k":"a","d":1,"x":0}"#), Err(ParseError::Malformed));
        assert_eq!(parse(br#"{"kfm":2,"k":"a","d":1}"#), Err(ParseError::NotOurs));
        assert_eq!(parse(br#""hello""#), Err(ParseError::NotOurs));
        assert_eq!(parse(b"\xff"), Err(ParseError::NotJson));
        assert_eq!(parse(&[b' '; 300]), Err(ParseError::TooLarge));
    }

    #[test]
    fn keymap_tables_agree() {
        for (name, u) in KEY_CODES {
            assert!(win_key_for_usage(Usage(*u)).is_some(), "{name} has no Windows key");
        }
    }

    #[test]
    fn toggle_turns_karabiner_route_on_only_with_a_link() {
        let mut b = bridge();
        assert_eq!(b.on_input(BridgeIn::ToggleRequest, 0), vec![Out::Refused]);
        assert!(!b.karabiner_remote());
        assert_eq!(b.on_link(true, ), vec![]);
        assert_eq!(b.on_input(BridgeIn::ToggleRequest, 0), vec![Out::Dest { dest: Dest::Windows, reason: Reason::Hotkey }, Out::SetRoute { remote: true }]);
    }

    fn on_windows() -> Bridge {
        let mut b = bridge();
        b.on_link(true);
        b.on_input(BridgeIn::ToggleRequest, 0);
        b
    }

    #[test]
    fn keys_and_modifiers_forward_post_simple_modification() {
        // Physical Cmd arrives from Karabiner as left_control (Rainy75 swap) -> Ctrl on Windows.
        let mut b = on_windows();
        assert_eq!(b.on_input(key("left_control", true), 1), vec![remote(hid::LCTRL, true)]);
        assert_eq!(b.on_input(key("c", true), 2), vec![remote(hid::C, true)]);
        assert_eq!(b.on_input(key("c", false), 3), vec![remote(hid::C, false)]);
        assert_eq!(b.on_input(key("left_control", false), 4), vec![remote(hid::LCTRL, false)]);
        assert_eq!(b.mac_keys_dropped, 0);
    }

    #[test]
    fn escape_releases_windows_keys_and_returns_to_mac() {
        let mut b = on_windows();
        b.on_input(key("left_shift", true), 1);
        b.on_input(key("a", true), 2);
        let out = b.on_input(BridgeIn::Escape, 3);
        assert_eq!(out, vec![remote(hid::A, false), remote(hid::LSHIFT, false), Out::Dest { dest: Dest::Mac, reason: Reason::Hotkey }]);
        assert!(!b.karabiner_remote()); // Karabiner already set it; no SetRoute needed
        // A late key-up (if Karabiner still forwards it) is swallowed, not double-released.
        assert_eq!(b.on_input(key("a", false), 4), vec![]);
    }

    #[test]
    fn link_loss_holds_with_karabiner_still_suppressing() {
        let mut b = on_windows();
        b.on_input(key("a", true), 1);
        assert_eq!(b.on_link(false), vec![remote(hid::A, false), Out::Dest { dest: Dest::Held, reason: Reason::LinkDown }]);
        assert!(b.karabiner_remote(), "held input must stay suppressed on the Mac");
        assert_eq!(b.on_input(key("b", true), 2), vec![]);
        assert_eq!(b.on_input(key("b", false), 3), vec![]);
        // Escape still works (Karabiner-native) and comes back to the Mac.
        assert_eq!(b.on_input(BridgeIn::Escape, 4), vec![Out::Dest { dest: Dest::Mac, reason: Reason::Hotkey }]);
    }

    #[test]
    fn opt_in_fallback_turns_route_off_and_surfaces_drops() {
        let r = Router::new(WindowsProfile::identity(), Hotkey::default()).with_policy(crate::router::LinkLossPolicy::FallbackToMac);
        let mut b = Bridge::new(r, Typematic::default());
        b.on_link(true);
        b.on_input(BridgeIn::ToggleRequest, 0);
        let out = b.on_link(false);
        assert_eq!(out.last(), Some(&Out::SetRoute { remote: false }));
        // A key Karabiner swallowed before the route change landed: the router
        // wanted the Mac but it cannot be delivered -> counted, never sent to Windows.
        assert_eq!(b.on_input(key("x", true), 5), vec![Out::MacKeyDropped]);
        assert_eq!(b.mac_keys_dropped, 1);
    }

    #[test]
    fn typematic_repeats_newest_non_modifier_only_while_held() {
        let mut b = on_windows();
        b.on_input(key("left_shift", true), 0);
        assert!(b.tick(10_000).is_empty(), "modifiers do not repeat");
        b.on_input(key("a", true), 1000);
        assert!(b.tick(1400).is_empty());
        assert_eq!(b.tick(1500), vec![Out::Remote { key: w(hid::A), down: true, repeat: true }]);
        assert!(b.tick(1520).is_empty());
        assert_eq!(b.tick(1533).len(), 1);
        b.on_input(key("a", false), 1540);
        assert!(b.tick(5000).is_empty());
    }

    // ---- stage 7b ----

    fn linked() -> Bridge { let mut b = bridge(); b.on_link(true); b }

    #[test]
    fn flow_decisions_move_the_route_and_release_on_return() {
        let mut b = linked();
        let out = b.on_flow(Target::Windows);
        assert!(out.contains(&Out::Dest { dest: Dest::Windows, reason: Reason::Flow }) && out.contains(&Out::SetRoute { remote: true }), "{out:?}");
        b.on_input(key("a", true), 0);
        let out = b.on_flow(Target::Mac);
        assert_eq!(out.iter().filter(|o| **o == remote(hid::A, false)).count(), 1, "the held key is released on Windows: {out:?}");
        assert!(out.contains(&Out::SetRoute { remote: false }));
        assert_eq!(b.on_flow(Target::Mac), vec![], "no change, nothing to do");
    }

    #[test]
    fn flow_to_windows_is_refused_while_the_link_is_down() {
        let mut b = bridge();
        assert_eq!(b.on_flow(Target::Windows), vec![Out::Refused]);
        assert!(!b.karabiner_remote());
    }

    #[test]
    fn hotkey_overrides_flow_until_the_next_flow_decision() {
        let mut b = linked();
        b.on_flow(Target::Windows);
        b.on_input(BridgeIn::Escape, 0); // Fn+F12 at route 1: Karabiner is already at route 0
        assert_eq!(b.dest(), Dest::Mac);
        // The follower only reports changes, so nothing moves until the mouse crosses again.
        let out = b.on_flow(Target::Mac);
        assert!(!out.iter().any(|o| matches!(o, Out::SetRoute { .. })));
        assert!(b.on_flow(Target::Windows).contains(&Out::SetRoute { remote: true }));
    }

    #[test]
    fn easy_switch_moves_like_flow_and_never_twice_with_rule_c() {
        let mut b = linked();
        let out = b.on_easy_switch(Target::Windows);
        assert!(out.contains(&Out::Dest { dest: Dest::Windows, reason: Reason::EasySwitch }) && out.contains(&Out::SetRoute { remote: true }), "{out:?}");
        assert_eq!(b.on_flow(Target::Windows), vec![], "rule C agreeing afterwards: nothing more");
        b.on_input(key("a", true), 0);
        let out = b.on_easy_switch(Target::Mac);
        assert!(out.contains(&remote(hid::A, false)) && out.contains(&Out::SetRoute { remote: false }), "{out:?}");
        assert_eq!(b.on_flow(Target::Mac), vec![]);
        // Fn+F12 wins until the MX4 next arrives or leaves (rule E only reports changes).
        b.on_input(BridgeIn::ToggleRequest, 0);
        assert_eq!(b.dest(), Dest::Windows);
        assert!(b.on_easy_switch(Target::Mac).contains(&Out::SetRoute { remote: false }));
    }

    #[test]
    fn easy_switch_to_windows_is_refused_by_secure_input_and_link_down() {
        let mut b = linked();
        b.on_secure_input(true);
        assert_eq!(b.on_easy_switch(Target::Windows), vec![], "password field / pause file: stays on the Mac");
        b.on_secure_input(false);
        b.on_link(false);
        assert_eq!(b.on_easy_switch(Target::Windows), vec![Out::Refused]);
        assert!(!b.karabiner_remote());
    }

    #[test]
    fn windows_gets_the_physical_modifiers_and_the_mac_path_is_unchanged() {
        // Rainy75 as configured (fixture = the real entries): Karabiner swaps
        // left_command <-> left_control; the map is read from karabiner.json.
        let mut c = crate::karabiner_config::tests::fixture();
        c["profiles"][1]["devices"][1]["simple_modifications"].as_array_mut().unwrap()
            .push(json!({"from": {"key_code": "right_control"}, "to": [{"key_code": "right_command"}]}));
        let m = crate::karabiner_config::windows_modifier_map(&c).unwrap();
        let mut b = Bridge::new(Router::new(WindowsProfile::undo_swaps(&m.map), Hotkey::default()), Typematic::default());
        b.on_link(true);
        b.on_flow(Target::Windows);
        // Physical Ctrl+C: Karabiner reports left_command + c -> Windows gets Ctrl+C.
        assert!(b.on_input(key("left_command", true), 0).contains(&remote(hid::LCTRL, true)));
        assert!(b.on_input(key("c", true), 0).contains(&remote(hid::C, true)));
        assert!(b.on_input(key("c", false), 0).contains(&remote(hid::C, false)));
        assert!(b.on_input(key("left_command", false), 0).contains(&remote(hid::LCTRL, false)));
        // Physical Win: reported as left_control -> Windows gets Win.
        assert!(b.on_input(key("left_control", true), 0).contains(&remote(hid::LGUI, true)));
        assert!(b.on_input(key("left_control", false), 0).contains(&remote(hid::LGUI, false)));
        // Physical right Ctrl: reported as right_command -> Windows gets right Ctrl.
        assert!(b.on_input(key("right_command", true), 0).contains(&remote(usage_of("right_control").unwrap(), true)));
        assert!(b.on_input(key("right_command", false), 0).contains(&remote(usage_of("right_control").unwrap(), false)));
        // Opt is not swapped: Alt.
        assert!(b.on_input(key("left_option", true), 0).contains(&remote(usage_of("left_option").unwrap(), true)));
        b.on_input(key("left_option", false), 0);
        // A modifier held across a switch to the Mac is released on Windows as the
        // same physical key it was pressed as.
        b.on_input(key("left_control", true), 0);
        let out = b.on_flow(Target::Mac);
        assert!(out.contains(&remote(hid::LGUI, false)), "{out:?}");
        // On the Mac nothing is sent: keys keep Karabiner's swap there.
        assert!(!b.on_input(key("left_command", true), 0).iter().any(|o| matches!(o, Out::Remote { .. })));
    }

    fn probe(enabled: bool, holder: Option<(&str, bool)>, locked: Option<bool>) -> SecureProbe {
        SecureProbe { enabled, holder: holder.map(|(name, loginwindow)| SecureHolder { pid: 405, name: name.into(), loginwindow }), locked }
    }

    #[test]
    fn k3_ignores_only_loginwindow_while_known_unlocked() {
        use SecureVerdict::*;
        assert_eq!(secure_verdict(&probe(true, Some(("loginwindow", true)), Some(false))), StuckLoginwindow);
        assert_eq!(secure_verdict(&probe(true, Some(("loginwindow", true)), Some(true))), Hold, "locked: the login field is real");
        assert_eq!(secure_verdict(&probe(true, Some(("loginwindow", true)), None)), Hold, "lock state unknown: safe side");
        assert_eq!(secure_verdict(&probe(true, Some(("ghostty", false)), Some(false))), Hold, "another app (terminal secure entry)");
        assert_eq!(secure_verdict(&probe(true, Some(("loginwindow", false)), Some(false))), Hold, "a process only named loginwindow");
        assert_eq!(secure_verdict(&probe(true, None, Some(false))), Hold, "holder unknown");
        assert_eq!(secure_verdict(&probe(false, None, None)), Off);
    }

    #[test]
    fn k3_stuck_loginwindow_lets_flow_move_and_a_real_holder_still_returns() {
        let mut b = linked();
        // The poll feeds on_secure_input(verdict == Hold): the stuck case is "not secure".
        let v = secure_verdict(&probe(true, Some(("loginwindow", true)), Some(false)));
        b.on_secure_input(v == SecureVerdict::Hold);
        assert!(b.on_flow(Target::Windows).contains(&Out::SetRoute { remote: true }));
        let v = secure_verdict(&probe(true, Some(("Terminal", false)), Some(false)));
        assert!(b.on_secure_input(v == SecureVerdict::Hold).contains(&Out::SetRoute { remote: false }));
        assert_eq!(b.on_flow(Target::Windows), vec![]);
    }

    #[test]
    fn resident_hold_fallback_returns_to_the_mac() {
        let mut b = linked();
        b.on_flow(Target::Windows);
        b.on_link(false);
        assert_eq!(b.dest(), Dest::Held);
        assert!(b.karabiner_remote(), "Hold keeps route 1 (keys swallowed, not sent to the Mac)");
        let out = b.fallback_to_mac();
        assert!(out.contains(&Out::SetRoute { remote: false }), "{out:?}");
        assert_eq!(b.dest(), Dest::Mac);
        // When the link returns it stays on the Mac until the next decision.
        assert!(!b.on_link(true).contains(&Out::SetRoute { remote: true }));
        assert!(b.on_flow(Target::Windows).contains(&Out::SetRoute { remote: true }));
    }

    #[test]
    fn k3_secure_input_returns_to_the_mac_and_blocks_windows() {
        let mut b = linked();
        b.on_flow(Target::Windows);
        b.on_input(key("left_shift", true), 0);
        let out = b.on_secure_input(true);
        assert!(out.contains(&remote(hid::LSHIFT, false)) && out.contains(&Out::SetRoute { remote: false }), "{out:?}");
        assert_eq!(b.on_flow(Target::Windows), vec![], "no Flow move while a password field is focused");
        assert_eq!(b.on_input(BridgeIn::ToggleRequest, 0), vec![], "nor by Fn+F12");
        assert_eq!(b.dest(), Dest::Mac);
        let out = b.on_secure_input(false);
        assert!(!out.contains(&Out::SetRoute { remote: true }), "ending it does not jump back to Windows: {out:?}");
        assert!(b.on_flow(Target::Windows).contains(&Out::SetRoute { remote: true }));
    }
}
