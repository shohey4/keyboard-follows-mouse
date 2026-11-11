//! User key configuration: the switch hotkey and per-target remaps of physical
//! keys, from one JSON file named by `KFM_KEYMAP` (unset = defaults: F13, no
//! remaps). `kfm-karabiner` (which generates the capture rule) and `kfm-mac`
//! (which sends) read the file separately: after changing the hotkey, apply
//! the rule again with the same file, or the installed rule keeps the old
//! hotkey. Every problem fails closed: the binaries refuse to start.
//!
//! ```json
//! {
//!   "hotkey": "f13",
//!   "remap": {
//!     "windows": [["caps_lock", "left_control"], ["left_control", "caps_lock"], ["right_option", "drop"]],
//!     "mac":     [["left_command", "left_control"], ["left_control", "left_command"]]
//!   }
//! }
//! ```
//!
//! Names are Karabiner-Elements `key_code` names of the forwarded keys (see
//! `karabiner::KEY_CODES`); a target may also be `"drop"`. For a Windows
//! receiver, remaps act on the physical key (Karabiner's own modifications for
//! the keyboard are undone first); for a Mac receiver they act on the key as
//! Karabiner reports it (the source Mac's modifications are kept). They are
//! resolved at key-down, so a held key's key-up always matches. Two keys may
//! not end up as the same key on one target (one key-up would release both).
//! Layout-level behaviour (dead keys, IME composition) stays with the receiving
//! OS: a remap changes which physical key the receiver sees, nothing more.

use serde_json::Value;

use crate::karabiner::{usage_of, KEY_CODES};
use crate::keymap::Action;
use crate::keys::Usage;

pub const KEYMAP_ENV: &str = "KFM_KEYMAP";

/// Keys that may be the switch hotkey: F13..F20. The hotkey is taken from the
/// source Mac as well (Karabiner turns it into the switch), so an ordinary key
/// such as Escape would stop working everywhere; these are unused by default
/// and most keyboards can send them through their firmware.
pub const EXTRA_HOTKEYS: &[&str] = &["f13", "f14", "f15", "f16", "f17", "f18", "f19", "f20"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetOs { Windows, Mac }

impl TargetOs {
    pub fn parse(s: &str) -> Result<TargetOs, String> {
        match s { "windows" => Ok(TargetOs::Windows), "mac" => Ok(TargetOs::Mac), _ => Err(format!("target OS must be windows or mac, got {s:?}")) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyConfig {
    /// Karabiner key_code of the switch / escape key.
    pub hotkey: String,
    pub windows: Vec<(Usage, Action)>,
    pub mac: Vec<(Usage, Action)>,
}

impl Default for KeyConfig {
    fn default() -> Self { KeyConfig { hotkey: "f13".into(), windows: vec![], mac: vec![] } }
}

impl KeyConfig {
    pub fn remaps(&self, target: TargetOs) -> &[(Usage, Action)] {
        match target { TargetOs::Windows => &self.windows, TargetOs::Mac => &self.mac }
    }
    /// The hotkey as a usage when it is one of the forwarded keys (it is then
    /// not forwarded); None for F13..F20.
    pub fn hotkey_usage(&self) -> Option<Usage> { usage_of(&self.hotkey) }
    /// The hotkey as the router knows it (F13..F20 have HID usages 0x68..0x6F).
    pub fn hotkey_router_usage(&self) -> Usage {
        self.hotkey_usage().unwrap_or_else(|| {
            let i = EXTRA_HOTKEYS.iter().position(|h| *h == self.hotkey).expect("validated hotkey");
            Usage(0x68 + i as u16)
        })
    }
}

pub fn parse(text: &str) -> Result<KeyConfig, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("keymap: {e}"))?;
    let obj = v.as_object().ok_or("keymap: the top level must be an object")?;
    for k in obj.keys() { if k != "hotkey" && k != "remap" { return Err(format!("keymap: unknown field {k:?}")); } }
    let mut c = KeyConfig::default();
    if let Some(h) = obj.get("hotkey") {
        let h = h.as_str().ok_or("keymap: hotkey must be a key_code name")?;
        if !EXTRA_HOTKEYS.contains(&h) {
            return Err(format!("keymap: hotkey {h:?} must be one of f13..f20 (an ordinary key would be lost on the Mac too)"));
        }
        c.hotkey = h.to_string();
    }
    if let Some(r) = obj.get("remap") {
        let r = r.as_object().ok_or("keymap: remap must be an object")?;
        for (target, list) in r {
            let t = TargetOs::parse(target).map_err(|e| format!("keymap: remap: {e}"))?;
            let list = list.as_array().ok_or_else(|| format!("keymap: remap.{target} must be a list of [from, to] pairs"))?;
            let mut out: Vec<(Usage, Action)> = vec![];
            for (i, pair) in list.iter().enumerate() {
                let p = pair.as_array().filter(|p| p.len() == 2).ok_or_else(|| format!("keymap: remap.{target}[{i}] must be [from, to]"))?;
                let (Some(from), Some(to)) = (p[0].as_str(), p[1].as_str()) else { return Err(format!("keymap: remap.{target}[{i}] must be two names")); };
                let f = usage_of(from).ok_or_else(|| format!("keymap: remap.{target}[{i}]: {from:?} is not a forwarded key"))?;
                if from == c.hotkey { return Err(format!("keymap: remap.{target}[{i}]: the hotkey {from:?} cannot be remapped")); }
                let a = if to == "drop" { Action::Drop } else {
                    Action::Key(usage_of(to).ok_or_else(|| format!("keymap: remap.{target}[{i}]: {to:?} is not a forwarded key or \"drop\""))?)
                };
                if out.iter().any(|(u, _)| *u == f) { return Err(format!("keymap: remap.{target}: {from:?} is mapped twice")); }
                out.push((f, a));
            }
            match t { TargetOs::Windows => c.windows = out, TargetOs::Mac => c.mac = out }
        }
    }
    // No two forwarded keys may end up as the same key on one target: the first
    // key-up would release the other one's key on the receiver.
    for (t, list) in [("windows", &c.windows), ("mac", &c.mac)] {
        let mut seen: std::collections::BTreeMap<Usage, &str> = Default::default();
        for (name, u) in KEY_CODES {
            let out = match list.iter().find(|(f, _)| f.0 == *u) { Some((_, Action::Key(to))) => *to, Some(_) => continue, None => Usage(*u) };
            if let Some(other) = seen.insert(out, name) {
                return Err(format!("keymap: remap.{t}: {other:?} and {name:?} would both be {:?}", crate::karabiner::name_of(out).unwrap_or("?")));
            }
        }
    }
    Ok(c)
}

/// `KFM_KEYMAP` (a path) parsed, or the defaults when unset. The binaries call
/// this at start-up and refuse to run on an error.
pub fn key_config_from_env() -> Result<KeyConfig, String> {
    match std::env::var_os(KEYMAP_ENV).filter(|p| !p.is_empty()) {
        None => Ok(KeyConfig::default()),
        Some(p) => parse(&std::fs::read_to_string(&p).map_err(|e| format!("{KEYMAP_ENV}={}: {e}", p.to_string_lossy()))?),
    }
}

/// The configured keys (see `key_config_from_env`).
pub fn key_config() -> &'static KeyConfig {
    static K: std::sync::OnceLock<KeyConfig> = std::sync::OnceLock::new();
    K.get_or_init(|| key_config_from_env().expect("keymap is checked at start-up"))
}

/// Names of the forwarded keys, for help text.
pub fn forwarded_names() -> impl Iterator<Item = &'static str> { KEY_CODES.iter().map(|(n, _)| *n) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::hid;

    #[test]
    fn defaults_and_a_full_example() {
        assert_eq!(parse("{}").unwrap(), KeyConfig::default());
        let c = parse(r#"{"hotkey":"f15","remap":{"windows":[["caps_lock","left_control"],["left_control","caps_lock"],["right_option","drop"]],"mac":[["left_command","left_control"],["left_control","left_command"]]}}"#).unwrap();
        assert_eq!(c.hotkey, "f15");
        assert_eq!(c.hotkey_usage(), None);
        assert_eq!(c.hotkey_router_usage(), Usage(0x6A));
        assert_eq!(c.windows, vec![(Usage(0x39), Action::Key(hid::LCTRL)), (hid::LCTRL, Action::Key(Usage(0x39))), (hid::RALT, Action::Drop)]);
        assert_eq!(c.remaps(TargetOs::Mac), &[(hid::LGUI, Action::Key(hid::LCTRL)), (hid::LCTRL, Action::Key(hid::LGUI))]);
    }

    /// The examples in README.md and SPEC.md must load as written.
    #[test]
    fn the_documented_examples_load() {
        for doc in [include_str!("../README.md"), include_str!("../SPEC.md")] {
            let mut found = 0;
            for block in doc.split("```json").skip(1) {
                let json = block.split("```").next().unwrap();
                if json.contains("\"remap\"") { parse(json).unwrap_or_else(|e| panic!("{e}\n{json}")); found += 1; }
            }
            assert!(found >= 1, "no keymap example found");
        }
    }

    #[test]
    fn an_ordinary_key_cannot_be_the_hotkey() {
        assert!(parse(r#"{"hotkey":"escape"}"#).is_err());
        assert!(parse(r#"{"hotkey":"right_option"}"#).is_err());
        assert_eq!(parse(r#"{"hotkey":"f20"}"#).unwrap().hotkey_router_usage(), Usage(0x6F));
    }

    #[test]
    fn two_keys_onto_one_target_are_rejected_but_swaps_are_fine() {
        assert!(parse(r#"{"remap":{"windows":[["caps_lock","left_control"]]}}"#).is_err(), "physical left_control still sends left_control");
        assert!(parse(r#"{"remap":{"windows":[["caps_lock","left_control"],["left_control","drop"]]}}"#).is_ok());
        assert!(parse(r#"{"remap":{"mac":[["caps_lock","left_control"],["left_control","caps_lock"]]}}"#).is_ok(), "a swap");
        assert!(parse(r#"{"remap":{"mac":[["a","c"],["b","c"]]}}"#).is_err());
    }

    #[test]
    fn invalid_configs_fail_closed() {
        for (bad, why) in [
            ("[]", "top level"), (r#"{"hotkey":"f12x"}"#, "unknown hotkey"), (r#"{"hotkey":"f21"}"#, "beyond f20"),
            (r#"{"hotkey":1}"#, "hotkey type"), (r#"{"extra":1}"#, "unknown field"), (r#"{"remap":{"linux":[]}}"#, "unknown target"),
            (r#"{"remap":{"windows":[["caps_lock"]]}}"#, "pair shape"), (r#"{"remap":{"windows":[["nope","a"]]}}"#, "unknown from"),
            (r#"{"remap":{"windows":[["a","nope"]]}}"#, "unknown to"), (r#"{"remap":{"windows":[["a","b"],["a","c"]]}}"#, "duplicate"),
            (r#"{"hotkey":"escape"}"#, "ordinary key as hotkey"),
            ("not json", "syntax"),
        ] {
            assert!(parse(bad).is_err(), "{why}: {bad}");
        }
    }
}
