//! Karabiner rule tool. Read-only unless `apply`/`restore` get their explicit
//! confirmation flag. Never sets `kfm_route` (the rule installs inactive).
//!
//!   kfm-karabiner preflight [--config PATH]
//!   kfm-karabiner plan      [--config PATH] [--endpoint SOCK] --out FILE
//!   kfm-karabiner rule      [--endpoint SOCK]
//!   kfm-karabiner apply     --config PATH [--endpoint SOCK] --confirm-backup-and-apply
//!   kfm-karabiner restore   --config PATH --backup FILE --confirm-restore

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use keyboard_follows_mouse::karabiner::rule;
use keyboard_follows_mouse::karabiner_config::{device_line, device_problem, installed_device, install, preflight, uninstall, verify};
use serde_json::Value;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn default_config() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/karabiner/karabiner.json")
}

fn read(path: &Path) -> Result<Value, String> {
    serde_json::from_str(&std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?).map_err(|e| format!("{}: {e}", path.display()))
}

/// Write via a temp file + rename so Karabiner never reads a half-written file.
fn write_atomic(path: &Path, v: &Value) -> Result<(), String> {
    let tmp = path.with_extension("kfm-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap() + "\n").map_err(|e| e.to_string())?;
    // Keep the original mode (karabiner.json is 0600), not the umask default.
    if let Ok(m) = std::fs::metadata(path) { std::fs::set_permissions(&tmp, m.permissions()).map_err(|e| e.to_string())?; }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn karabiner_dir_guard(out: &Path) -> Result<(), String> {
    let abs = std::path::absolute(out).map_err(|e| e.to_string())?;
    let kdir = default_config().parent().unwrap().to_path_buf();
    if abs.starts_with(&kdir) { return Err(format!("refusing to write {} inside {}: use `apply`", abs.display(), kdir.display())); }
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    let config = arg(args, "--config").map(PathBuf::from).unwrap_or_else(default_config);
    let endpoint = arg(args, "--endpoint");
    match args.first().map(String::as_str) {
        Some("preflight") => {
            let v = read(&config)?;
            let mut p = preflight(&v);
            // The installed rule must use the configured hotkey, or the recovery text and the real escape key disagree.
            let want = &keyboard_follows_mouse::remap::key_config().hotkey;
            if let Some(i) = p.our_rule_index {
                let got = v["profiles"].as_array().and_then(|ps| ps.iter().find(|p| p["selected"] == serde_json::json!(true)))
                    .and_then(|p| p["complex_modifications"]["rules"][i]["manipulators"][0]["from"]["key_code"].as_str()).map(String::from);
                println!("hotkey: configured {want}, installed rule {}", got.as_deref().unwrap_or("?"));
                if got.as_deref() != Some(want.as_str()) { p.problems.push(format!("the installed rule's hotkey is not the configured {want}: apply the rule again with the same KFM_KEYMAP")); }
                // The rule captures one keyboard: a changed KFM_KEYBOARD_* with the old rule would leave the old keyboard captured.
                let (configured, installed) = (keyboard_follows_mouse::karabiner::keyboard(), installed_device(&v, i));
                println!("{}", device_line(configured, installed));
                if let Some(m) = device_problem(configured, installed) { p.problems.push(m); }
            }
            println!("config: {}", config.display());
            println!("selected profile: {:?}", p.selected_profile);
            println!("complex rules: {} (ours at {:?})", p.rules, p.our_rule_index);
            println!("keyboard device entry: {} (simple modifications: {})", p.rainy75_device_entry, p.rainy75_simple_modifications);
            println!("other rules limited to the keyboard: {:?}", p.other_rainy75_rules);
            println!("problems: {:?}", p.problems);
            if p.ok() { Ok(()) } else { Err("preflight found problems".into()) }
        }
        Some("rule") => { println!("{}", serde_json::to_string_pretty(&rule(endpoint.as_deref())).unwrap()); Ok(()) }
        Some("plan") => {
            let out = PathBuf::from(arg(args, "--out").ok_or("--out FILE is required")?);
            karabiner_dir_guard(&out)?;
            let before = read(&config)?;
            let after = install(&before, endpoint.as_deref())?;
            write_atomic(&out, &after)?;
            println!("plan written to {} (Karabiner not touched)", out.display());
            println!("verified: only one rule added at the top of the selected profile; route variable not set");
            println!("uninstall restores the original exactly: {}", uninstall(&after) == uninstall(&before));
            Ok(())
        }
        Some("apply") => {
            if !args.iter().any(|a| a == "--confirm-backup-and-apply") { return Err("apply needs --confirm-backup-and-apply".into()); }
            let before = read(&config)?;
            let after = install(&before, endpoint.as_deref())?;
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            let backup = config.with_extension(format!("json.kfm-backup-{stamp}"));
            std::fs::OpenOptions::new().write(true).create_new(true).open(&backup).map_err(|e| format!("backup: {e}"))?;
            std::fs::copy(&config, &backup).map_err(|e| format!("backup: {e}"))?;
            if read(&backup)? != before { return Err("backup does not match the original; nothing applied".into()); }
            write_atomic(&config, &after)?;
            verify(&before, &read(&config)?)?;
            println!("backup: {}", backup.display());
            println!("applied; kfm_route is unset (= 0): the rule is inactive until the bridge turns it on");
            println!("undo: kfm-karabiner restore --config {} --backup {} --confirm-restore", config.display(), backup.display());
            Ok(())
        }
        Some("restore") => {
            if !args.iter().any(|a| a == "--confirm-restore") { return Err("restore needs --confirm-restore".into()); }
            let backup = PathBuf::from(arg(args, "--backup").ok_or("--backup FILE is required")?);
            read(&backup)?; // must parse; then restore the exact bytes
            let tmp = config.with_extension("kfm-tmp");
            std::fs::copy(&backup, &tmp).map_err(|e| e.to_string())?;
            if let Ok(m) = std::fs::metadata(&config) { std::fs::set_permissions(&tmp, m.permissions()).map_err(|e| e.to_string())?; }
            std::fs::rename(&tmp, &config).map_err(|e| e.to_string())?;
            println!("restored {} from {}", config.display(), backup.display());
            Ok(())
        }
        _ => Err("usage: kfm-karabiner preflight|plan|rule|apply|restore (see source header)".into()),
    }
}

fn main() -> ExitCode {
    if let Err(e) = keyboard_follows_mouse::karabiner::keyboard_from_env() { eprintln!("kfm-karabiner: {e}"); return ExitCode::FAILURE; }
    if let Err(e) = keyboard_follows_mouse::remap::key_config_from_env() { eprintln!("kfm-karabiner: {e}"); return ExitCode::FAILURE; }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("kfm-karabiner: {e}"); ExitCode::FAILURE }
    }
}
