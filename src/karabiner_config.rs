//! Reversible installation of the generated rule into karabiner.json (pure
//! JSON transforms; the `kfm-karabiner` binary does the file I/O).
//!
//! The only allowed change is inserting (or replacing) our one rule at the top
//! of the selected profile's complex_modifications.rules. `verify` proves that
//! by comparing the result with the original minus our rule; anything else
//! (simple modifications, other rules, devices, other profiles) must be equal.

use serde_json::{Value, json};

use crate::karabiner::{keyboard, rule, usage_of, Keyboard, ROUTE_VAR, TESTED_KEYBOARD};
use crate::keys::Usage;

/// Our rule is recognised by this description prefix.
pub const MARKER: &str = "keyboard-follows-mouse:";

#[derive(Debug, PartialEq, Eq)]
pub struct Preflight {
    pub selected_profile: Option<String>,
    pub rules: usize,
    pub our_rule_index: Option<usize>,
    pub rainy75_device_entry: bool,
    pub rainy75_simple_modifications: usize,
    /// Rules other than ours that mention our variable.
    pub variable_conflicts: Vec<String>,
    /// Rules other than ours limited to Rainy75 (would interact).
    pub other_rainy75_rules: Vec<String>,
    pub problems: Vec<String>,
}

impl Preflight {
    pub fn ok(&self) -> bool { self.problems.is_empty() }
}

fn selected(config: &Value) -> Option<(usize, &Value)> {
    config["profiles"].as_array()?.iter().enumerate().find(|(_, p)| p["selected"] == json!(true))
}

fn is_ours(rule: &Value) -> bool {
    rule["description"].as_str().is_some_and(|d| d.starts_with(MARKER))
}

fn mentions_rainy75(v: &Value) -> bool {
    let k = keyboard();
    v.to_string().contains(&format!("\"vendor_id\":{}", k.vendor_id)) && v.to_string().contains(&format!("\"product_id\":{}", k.product_id))
}

/// The keyboard (vendor_id, product_id) that every manipulator of the installed rule `rule_index` of the selected
/// profile is limited to. None when a manipulator has no single device condition or they disagree: the rule cannot
/// be trusted to capture the configured keyboard.
pub fn installed_device(config: &Value, rule_index: usize) -> Option<(u64, u64)> {
    let (_, profile) = selected(config)?;
    let ms = profile["complex_modifications"]["rules"][rule_index]["manipulators"].as_array()?;
    let mut found: Option<(u64, u64)> = None;
    for m in ms {
        let mut devices = m["conditions"].as_array()?.iter().filter(|c| c["type"] == "device_if");
        let ids = devices.next()?["identifiers"].as_array()?;
        if devices.next().is_some() || ids.len() != 1 { return None; }
        let d = (ids[0]["vendor_id"].as_u64()?, ids[0]["product_id"].as_u64()?);
        if found.is_some_and(|f| f != d) { return None; }
        found = Some(d);
    }
    found
}

fn hex_pair(k: Option<(u64, u64)>) -> String { k.map_or("?".into(), |(v, p)| format!("{v:#06x}:{p:#06x}")) }

/// The `device:` line of `kfm-karabiner preflight` (parsed by `kfm-config`).
pub fn device_line(configured: Keyboard, installed: Option<(u64, u64)>) -> String {
    format!("device: configured {}, installed rule {}", hex_pair(Some((configured.vendor_id, configured.product_id))), hex_pair(installed))
}

/// A preflight problem when the installed rule would capture another keyboard than the configured one (or cannot be read).
pub fn device_problem(configured: Keyboard, installed: Option<(u64, u64)>) -> Option<String> {
    (installed != Some((configured.vendor_id, configured.product_id))).then(|| format!(
        "the installed rule is limited to keyboard {} but KFM_KEYBOARD_VENDOR_ID/PRODUCT_ID say {}: apply the rule again with the same settings",
        hex_pair(installed), hex_pair(Some((configured.vendor_id, configured.product_id)))))
}

pub fn preflight(config: &Value) -> Preflight {
    let mut p = Preflight {
        selected_profile: None, rules: 0, our_rule_index: None, rainy75_device_entry: false,
        rainy75_simple_modifications: 0, variable_conflicts: vec![], other_rainy75_rules: vec![], problems: vec![],
    };
    let Some((_, profile)) = selected(config) else {
        p.problems.push("no selected profile".into());
        return p;
    };
    p.selected_profile = profile["name"].as_str().map(String::from);
    let rules = profile["complex_modifications"]["rules"].as_array().cloned().unwrap_or_default();
    p.rules = rules.len();
    for (i, r) in rules.iter().enumerate() {
        let name = r["description"].as_str().unwrap_or("(no description)").to_string();
        if is_ours(r) {
            if p.our_rule_index.is_some() { p.problems.push("our rule appears more than once".into()); }
            p.our_rule_index = Some(i);
            continue;
        }
        if r.to_string().contains(&format!("\"{ROUTE_VAR}\"")) { p.variable_conflicts.push(name.clone()); }
        if mentions_rainy75(r) { p.other_rainy75_rules.push(name); }
    }
    if p.our_rule_index.is_some_and(|i| i != 0) { p.problems.push("our rule is not first".into()); }
    if !p.variable_conflicts.is_empty() { p.problems.push(format!("another rule uses {ROUTE_VAR}")); }
    for d in profile["devices"].as_array().into_iter().flatten() {
        let id = &d["identifiers"];
        if id["vendor_id"] == json!(keyboard().vendor_id) && id["product_id"] == json!(keyboard().product_id) {
            p.rainy75_device_entry = true;
            p.rainy75_simple_modifications = d["simple_modifications"].as_array().map_or(0, |a| a.len());
            if d["ignore"] == json!(true) { p.problems.push("the keyboard is ignored by Karabiner (mode A needs Karabiner to grab it)".into()); }
        }
    }
    p
}

/// The config with our rule first (replacing an older copy). Errors if the
/// preflight has problems other than a misplaced/duplicated copy of our rule.
pub fn install(config: &Value, endpoint: Option<&str>) -> Result<Value, String> {
    let pf = preflight(config);
    let blocking: Vec<_> = pf.problems.iter().filter(|m| !m.starts_with("our rule")).collect();
    if !blocking.is_empty() { return Err(format!("preflight: {blocking:?}")); }
    let mut out = uninstall(config);
    let (i, _) = selected(&out).ok_or("no selected profile")?;
    let profile = &mut out["profiles"][i];
    if profile["complex_modifications"].is_null() { profile["complex_modifications"] = json!({}); }
    let cm = &mut profile["complex_modifications"];
    if cm["rules"].is_null() { cm["rules"] = json!([]); }
    cm["rules"].as_array_mut().ok_or("rules is not an array")?.insert(0, rule(endpoint));
    verify(config, &out)?;
    Ok(out)
}

/// The config without our rule; everything else untouched.
pub fn uninstall(config: &Value) -> Value {
    let mut out = config.clone();
    for p in out["profiles"].as_array_mut().into_iter().flatten() {
        if let Some(rules) = p.get_mut("complex_modifications").and_then(|c| c.get_mut("rules")).and_then(Value::as_array_mut) {
            rules.retain(|r| !is_ours(r));
        }
    }
    out
}

/// `after` must equal `before` except for exactly one copy of our rule at the
/// top of the selected profile.
pub fn verify(before: &Value, after: &Value) -> Result<(), String> {
    let pf = preflight(after);
    if pf.our_rule_index != Some(0) || !pf.problems.is_empty() { return Err(format!("our rule not uniquely first: {:?}", pf.problems)); }
    let mut stripped = uninstall(after);
    let mut original = uninstall(before);
    // `install` may have created empty containers; normalise them away.
    for v in [&mut stripped, &mut original] {
        for p in v["profiles"].as_array_mut().into_iter().flatten() {
            // Read through `get`: IndexMut on a missing key would insert null.
            if p.get("complex_modifications").and_then(|c| c.get("rules")) == Some(&json!([])) {
                let cm = p["complex_modifications"].as_object_mut().unwrap();
                cm.remove("rules");
                if cm.is_empty() { p.as_object_mut().unwrap().remove("complex_modifications"); }
            }
        }
    }
    if stripped != original { return Err("something other than our rule changed".into()); }
    // Our rule touches Rainy75 only.
    let ours = &after["profiles"][selected(after).unwrap().0]["complex_modifications"]["rules"][0];
    for m in ours["manipulators"].as_array().ok_or("rule has no manipulators")? {
        if !mentions_rainy75(&m["conditions"]) { return Err("a manipulator is not limited to the keyboard".into()); }
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
pub struct ModifierMap {
    /// (key Karabiner reports, physical key), for Windows.
    pub map: Vec<(Usage, Usage)>,
    /// Entries left alone, for the start-up log.
    pub notes: Vec<String>,
    /// The explicit Rainy75 facts below disagree with karabiner.json.
    pub warnings: Vec<String>,
}

/// Known hardware fact of the tested Rainy75: it has no physical right Win (right
/// Cmd) key, so a reported right_command can only be the physical right Ctrl
/// (Karabiner maps right_control -> right_command). The physical key is left
/// out of the sources, and the result is checked against this pairing.
const RAINY75_ABSENT: &str = "right_command";
const RAINY75_EXPECT: (&str, &str) = ("right_command", "right_control");

/// The eight modifier keys, as Karabiner names them.
const MODIFIERS: [&str; 8] = ["left_control", "left_shift", "left_option", "left_command", "right_control", "right_shift", "right_option", "right_command"];

/// Rainy75's modifier swaps, inverted: (key Karabiner reports, physical key).
/// Mode A sees keys after the simple modifications, so sending the physical key
/// to Windows undoes the swap there while the Mac keeps it. Profile-level
/// entries apply unless the Rainy75 device entry has the same `from`. Only
/// one-to-one modifier-to-modifier entries count; anything else, and a reported
/// key that two physical keys arrive as, is reported in the notes and left alone.
/// Keys Rainy75 does not have are not sources (see `RAINY75_ABSENT`).
pub fn windows_modifier_map(config: &Value) -> Result<ModifierMap, String> {
    let (_, profile) = selected(config).ok_or("no selected profile")?;
    let mut notes = vec![];
    let mut swap: std::collections::BTreeMap<&str, String> = Default::default();
    let mut read = |list: &Value, whose: &str| {
        for e in list.as_array().into_iter().flatten() {
            let from = e["from"]["key_code"].as_str();
            let to = e["to"].as_array().filter(|t| t.len() == 1).and_then(|t| t[0]["key_code"].as_str().filter(|_| t[0].as_object().is_some_and(|o| o.len() == 1)));
            match (from.and_then(|f| MODIFIERS.iter().find(|m| **m == f)), to) {
                (Some(f), Some(t)) if MODIFIERS.contains(&t) => { swap.insert(f, t.to_string()); }
                (Some(f), _) => { swap.remove(f); notes.push(format!("{whose}: {f} maps to something other than one modifier; left as is")); }
                _ => {}
            }
        }
    };
    read(&profile["simple_modifications"], "profile");
    let devices: Vec<&Value> = profile["devices"].as_array().into_iter().flatten()
        .filter(|d| d["identifiers"]["vendor_id"] == json!(keyboard().vendor_id) && d["identifiers"]["product_id"] == json!(keyboard().product_id)).collect();
    if devices.len() > 1 { return Err("more than one device entry for the keyboard".into()); }
    if let Some(d) = devices.first() { read(&d["simple_modifications"], "keyboard"); }
    // Reported key -> the physical keys that arrive as it. Only a reported key
    // with exactly one physical source can be undone; an ambiguous one is sent
    // as reported (with a note), since its physical key cannot be told.
    let mut sources: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    // The Rainy75 facts apply only to the tested keyboard; another keyboard has all eight modifiers.
    let rainy75 = keyboard() == TESTED_KEYBOARD;
    for m in MODIFIERS.into_iter().filter(|m| !rainy75 || *m != RAINY75_ABSENT) { sources.entry(swap.get(m).map_or(m, |t| t.as_str())).or_default().push(m); }
    let (want, want_from) = RAINY75_EXPECT;
    let mut warnings = vec![];
    match sources.get(want).map(Vec::as_slice) {
        _ if !rainy75 => {}
        Some([p]) if *p == want_from => {}
        got => warnings.push(format!("expected {want} to be the physical {want_from} (Rainy75 has no {RAINY75_ABSENT} key), but karabiner.json makes it {}; using karabiner.json",
            got.map_or("arrive from no key".to_string(), |g| format!("arrive from {}", g.join(" and "))))),
    }
    let mut map = vec![];
    for (got, from) in sources {
        match from.as_slice() {
            [p] if *p != got => map.push((usage_of(got).ok_or("unknown key")?, usage_of(p).ok_or("unknown key")?)),
            [_] => {}
            many => notes.push(format!("{} all arrive as {got}; {got} is sent as is", many.join(" and "))),
        }
    }
    map.sort();
    Ok(ModifierMap { map, notes, warnings })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::karabiner::{RAINY75_PRODUCT_ID, RAINY75_VENDOR_ID};

    /// Synthetic config with the same shape as the real one (not a copy of it):
    /// two profiles, global simple mods, two device entries, five rules.
    pub fn fixture() -> Value {
        json!({
            "global": {"show_in_menu_bar": true},
            "profiles": [
                {"name": "Other", "complex_modifications": {"rules": [{"description": "other profile rule", "manipulators": []}]}},
                {
                    "name": "Default profile", "selected": true,
                    "simple_modifications": [
                        {"from": {"key_code": "left_command"}, "to": [{"key_code": "left_option"}]}
                    ],
                    "devices": [
                        {"identifiers": {"is_keyboard": true, "vendor_id": 7805, "product_id": 12570}, "simple_modifications": []},
                        {"identifiers": {"is_keyboard": true, "vendor_id": RAINY75_VENDOR_ID, "product_id": RAINY75_PRODUCT_ID},
                         "simple_modifications": [
                            {"from": {"key_code": "left_command"}, "to": [{"key_code": "left_control"}]},
                            {"from": {"key_code": "left_control"}, "to": [{"key_code": "left_command"}]},
                            {"from": {"key_code": "left_option"}, "to": [{"key_code": "left_option"}]}
                         ]}
                    ],
                    "complex_modifications": {"rules": [
                        {"description": "CapsLock -> kana", "manipulators": [{"type": "basic", "from": {"key_code": "caps_lock"}, "to": [{"key_code": "japanese_kana"}]}]},
                        {"description": "r2", "manipulators": []}, {"description": "r3", "manipulators": []},
                        {"description": "r4", "manipulators": []}, {"description": "r5", "manipulators": []}
                    ]}
                }
            ]
        })
    }

    /// The installed rule's device ids, and the preflight problem when they are not the configured keyboard.
    #[test]
    fn installed_rule_device_is_compared_with_the_configured_keyboard() {
        let installed = install(&fixture(), None).unwrap();
        // install() ran with the default (tested) keyboard
        assert_eq!(installed_device(&installed, 0), Some((RAINY75_VENDOR_ID, RAINY75_PRODUCT_ID)));
        assert_eq!(device_problem(TESTED_KEYBOARD, installed_device(&installed, 0)), None, "exact match");
        let other = Keyboard { vendor_id: 0x1234, product_id: 0x5678 };
        let p = device_problem(other, installed_device(&installed, 0)).expect("same hotkey, other keyboard: a problem");
        assert!(p.contains("0x320f:0x5055") && p.contains("0x1234:0x5678"), "{p}");
        assert_eq!(device_line(other, installed_device(&installed, 0)), "device: configured 0x1234:0x5678, installed rule 0x320f:0x5055");
        // a rule written for another keyboard, checked against the default (config has no ids): not current
        let mut moved = installed.clone();
        for m in moved["profiles"][1]["complex_modifications"]["rules"][0]["manipulators"].as_array_mut().unwrap() {
            for c in m["conditions"].as_array_mut().unwrap().iter_mut().filter(|c| c["type"] == "device_if") { c["identifiers"] = json!([{"vendor_id": 0x1234, "product_id": 0x5678}]); }
        }
        assert_eq!(installed_device(&moved, 0), Some((0x1234, 0x5678)));
        assert!(device_problem(TESTED_KEYBOARD, installed_device(&moved, 0)).is_some());
        assert_eq!(device_problem(other, installed_device(&moved, 0)), None, "ids set in the rule and in the config, equal");
        // one manipulator disagreeing, a missing device condition, a missing rule: unreadable, so a problem
        let mut mixed = installed.clone();
        mixed["profiles"][1]["complex_modifications"]["rules"][0]["manipulators"][0]["conditions"][0]["identifiers"] = json!([{"vendor_id": 1, "product_id": 2}]);
        assert_eq!(installed_device(&mixed, 0), None);
        let mut bare = installed.clone();
        bare["profiles"][1]["complex_modifications"]["rules"][0]["manipulators"][0]["conditions"] = json!([]);
        assert_eq!(installed_device(&bare, 0), None);
        assert_eq!(installed_device(&installed, 99), None);
        assert!(device_problem(TESTED_KEYBOARD, None).is_some());
        assert!(device_line(TESTED_KEYBOARD, None).ends_with("installed rule ?"));
    }

    #[test]
    fn windows_modifier_map_inverts_the_rainy75_swap() {
        let m = windows_modifier_map(&fixture()).unwrap();
        let u = |k| usage_of(k).unwrap();
        // The fixture's Rainy75 entry swaps left Cmd and Ctrl and keeps Opt; the
        // profile-level entries are overridden for those keys. It has no right
        // swap, so the explicit right_command pairing does not hold: a warning.
        assert_eq!(m.map, vec![(u("left_control"), u("left_command")), (u("left_command"), u("left_control"))]);
        assert!(m.notes.is_empty(), "{:?}", m.notes);
        assert_eq!(m.warnings.len(), 1, "{:?}", m.warnings);
        assert!(m.warnings[0].contains("arrive from no key"), "{:?}", m.warnings);
    }

    #[test]
    fn windows_modifier_map_leaves_ambiguous_and_non_modifier_entries_alone() {
        let with = |sm: Value| { let mut c = fixture(); c["profiles"][1]["devices"][1]["simple_modifications"] = sm; c };
        // The tested Rainy75 entry maps right_control -> right_command.
        // Rainy75 has no right Cmd key, so right_command is the physical right Ctrl.
        let c = with(json!([
            {"from": {"key_code": "left_command"}, "to": [{"key_code": "left_control"}]},
            {"from": {"key_code": "left_control"}, "to": [{"key_code": "left_command"}]},
            {"from": {"key_code": "right_control"}, "to": [{"key_code": "right_command"}]},
        ]));
        let m = windows_modifier_map(&c).unwrap();
        let u = |k| usage_of(k).unwrap();
        assert_eq!(m.map, vec![(u("left_control"), u("left_command")), (u("left_command"), u("left_control")), (u("right_command"), u("right_control"))]);
        assert!(m.notes.is_empty() && m.warnings.is_empty(), "{m:?}");
        // If Karabiner later maps another key to right_command, the explicit pairing
        // disagrees: warn, and since right_option is then its only source, use that.
        let c = with(json!([{"from": {"key_code": "right_option"}, "to": [{"key_code": "right_command"}]}]));
        let m = windows_modifier_map(&c).unwrap();
        assert!(m.map.contains(&(u("right_command"), u("right_option"))), "{m:?}");
        assert!(m.warnings[0].contains("arrive from right_option"), "{m:?}");
        // Two real sources for one reported key stay ambiguous: sent as is.
        let c = with(json!([{"from": {"key_code": "right_shift"}, "to": [{"key_code": "right_control"}]}]));
        let m = windows_modifier_map(&c).unwrap();
        assert!(!m.map.iter().any(|(r, _)| *r == u("right_control")), "{m:?}");
        // (Here the profile's left_command -> left_option is no longer overridden, so left_option is ambiguous too.)
        assert!(m.notes.contains(&"right_control and right_shift all arrive as right_control; right_control is sent as is".to_string()), "{m:?}");
        let c = with(json!([
            {"from": {"key_code": "application"}, "to": [{"apple_vendor_top_case_key_code": "keyboard_fn"}]},
            {"from": {"key_code": "left_command"}, "to": [{"key_code": "left_command"}]},
            {"from": {"key_code": "left_control"}, "to": [{"key_code": "left_control"}]},
            {"from": {"key_code": "left_option"}, "to": [{"key_code": "left_option", "modifiers": ["shift"]}]},
        ]));
        let m = windows_modifier_map(&c).unwrap();
        assert_eq!(m.map, vec![], "identity entries, the non-modifier one, and the one with extra fields change nothing");
        assert_eq!(m.notes.len(), 1, "{:?}", m.notes);
        assert!(windows_modifier_map(&json!({"profiles": []})).is_err());
    }

    #[test]
    fn preflight_reads_the_shape() {
        let p = preflight(&fixture());
        assert_eq!(p.selected_profile.as_deref(), Some("Default profile"));
        assert_eq!((p.rules, p.rainy75_device_entry, p.rainy75_simple_modifications), (5, true, 3));
        assert_eq!(p.our_rule_index, None);
        assert!(p.ok(), "{:?}", p.problems);
    }

    #[test]
    fn install_inserts_only_our_rule_first_and_is_idempotent() {
        let before = fixture();
        let after = install(&before, None).unwrap();
        let rules = &after["profiles"][1]["complex_modifications"]["rules"];
        assert_eq!(rules.as_array().unwrap().len(), 6);
        assert!(is_ours(&rules[0]));
        assert_eq!(&rules.as_array().unwrap()[1..], &before["profiles"][1]["complex_modifications"]["rules"].as_array().unwrap()[..]);
        // Simple modifications, devices, other profile, globals: identical.
        for k in ["simple_modifications", "devices", "name"] { assert_eq!(after["profiles"][1][k], before["profiles"][1][k]); }
        assert_eq!(after["profiles"][0], before["profiles"][0]);
        assert_eq!(after["global"], before["global"]);
        // Installing again replaces, never duplicates.
        let again = install(&after, Some("/x.sock")).unwrap();
        assert_eq!(again["profiles"][1]["complex_modifications"]["rules"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn uninstall_restores_the_original_exactly() {
        let before = fixture();
        assert_eq!(uninstall(&install(&before, None).unwrap()), before);
    }

    #[test]
    fn verify_rejects_any_other_change() {
        let before = fixture();
        let mut after = install(&before, None).unwrap();
        after["profiles"][1]["devices"][1]["simple_modifications"][0]["to"][0]["key_code"] = json!("left_option");
        assert!(verify(&before, &after).is_err());
        let mut after = install(&before, None).unwrap();
        after["profiles"][1]["complex_modifications"]["rules"].as_array_mut().unwrap().swap(0, 1);
        assert!(verify(&before, &after).is_err());
        let mut after = install(&before, None).unwrap();
        after["profiles"][1]["complex_modifications"]["rules"][0]["manipulators"][2]["conditions"][0]["identifiers"][0]["product_id"] = json!(1);
        assert!(verify(&before, &after).is_err());
    }

    #[test]
    fn blocking_problems_stop_install() {
        let mut c = fixture();
        c["profiles"][1]["complex_modifications"]["rules"][1]["manipulators"] = json!([{"conditions": [{"type": "variable_if", "name": ROUTE_VAR, "value": 1}]}]);
        assert!(install(&c, None).unwrap_err().contains(ROUTE_VAR));
        let mut c = fixture();
        c["profiles"][1]["devices"][1]["ignore"] = json!(true);
        assert!(install(&c, None).is_err());
        let mut c = fixture();
        c["profiles"][1]["selected"] = json!(false);
        assert!(install(&c, None).is_err());
    }

    #[test]
    fn works_without_existing_rules() {
        let mut c = fixture();
        c["profiles"][1].as_object_mut().unwrap().remove("complex_modifications");
        let after = install(&c, None).unwrap();
        assert_eq!(uninstall(&after)["profiles"][1]["complex_modifications"]["rules"], json!([]));
        verify(&c, &after).unwrap();
    }
}
