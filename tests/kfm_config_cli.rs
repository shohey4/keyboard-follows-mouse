//! The built `kfm-config` against a settings folder whose path contains spaces. Only temp folders are
//! touched; HOME points at an empty temp folder so no real installation or Karabiner file is read.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env { root: PathBuf }

impl Env {
    fn new(tag: &str) -> Env {
        let root = std::env::temp_dir().join(format!("kfm config {tag} {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home dir")).unwrap();
        Env { root }
    }
    fn local(&self) -> PathBuf { self.root.join("my local") }
    fn installed(&self) -> PathBuf { self.root.join("installed copy") }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kfm-config")).args(args).arg("--dir").arg(self.local()).arg("--installed-dir").arg(self.installed())
            .env("HOME", self.root.join("home dir")).env("USERPROFILE", self.root.join("home dir")).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(o.status.success(), "{args:?}\n{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }
    fn fails(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(!o.status.success(), "{args:?} should fail");
        format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
    }
    fn read(&self, f: &str) -> String { std::fs::read_to_string(self.local().join(f)).unwrap() }
}
impl Drop for Env { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); } }

fn key_arg(e: &Env) -> String { e.local().join("kfm-psk.key").to_string_lossy().into_owned() }

#[test]
fn receiver_flow_in_a_folder_with_spaces() {
    let e = Env::new("receiver");
    e.ok(&["init", "--role", "receiver"]);
    assert!(e.fails(&["init", "--role", "receiver"]).contains("already exists"), "never overwrites");
    let before = e.read("config.sh");
    // dry run and invalid changes write nothing
    assert!(e.ok(&["set", "KFM_PEER", "192.0.2.20:47502", "--dry-run"]).contains("dry run"));
    assert_eq!(e.read("config.sh"), before);
    assert!(e.fails(&["set", "KFM_PEER", "not-an-address"]).contains("IP:PORT"));
    assert!(e.fails(&["set", "KFM_FLOW_EDGE", "up"]).contains("left, right, top or bottom"));
    assert_eq!(e.read("config.sh"), before);
    // incomplete: validate refuses, run-args refuses
    assert!(e.fails(&["validate"]).contains("MISSING"));
    assert!(e.fails(&["run-args", "receiver"]).contains("not complete"));
    e.ok(&["set", "KFM_PEER", "192.0.2.20:47502", "KFM_BIND", "192.0.2.10:47501", "KFM_FLOW_EDGE", "top"]);
    assert!(e.read("config.sh").contains("KFM_KEY='"), "a path with spaces is single-quoted");
    assert!(e.ok(&["validate"]).contains("validated"));
    let args = e.ok(&["run-args", "receiver", "--shell", "sh"]);
    assert_eq!(args.trim(), format!("--bind 192.0.2.10:47501 --peer 192.0.2.20:47502 --key-file '{}' --flow-edge top --flow-display all", key_arg(&e)));
    assert!(e.ok(&["run-args", "receiver", "--shell", "powershell"]).contains(&format!("--key-file '{}'", key_arg(&e))));
    assert!(e.fails(&["run-args", "windows-source"]).contains("not windows-source"));
    // masked unless asked
    let shown = e.ok(&["show"]);
    assert!(!shown.contains("192.0.2") && !shown.contains("my local"), "{shown}");
    assert!(e.ok(&["show", "--reveal"]).contains("192.0.2.20:47502"));
    // the plan only prints
    // On a Mac that already has another kfm/r75 launchd job, apply prints only the STOP message and exits 1.
    let o = e.run(&["apply"]);
    let plan = String::from_utf8_lossy(&o.stdout).into_owned();
    if o.status.success() { assert!(plan.contains("kfm-receive") && plan.contains("--enable-inject") && plan.contains("rollback"), "{plan}"); }
    else { assert!(plan.contains("STOP") && !plan.contains("--enable-inject"), "{plan}"); }
    assert!(!Path::new(&key_arg(&e)).exists(), "nothing created a key");
}

#[test]
fn key_edits_backup_status_and_restore() {
    let e = Env::new("keys");
    e.ok(&["init", "--role", "mac-source"]);
    e.ok(&["set", "KFM_PEER", "192.0.2.10:47501", "KFM_BIND", "192.0.2.20:47502"]);
    assert!(e.fails(&["map", "windows", "caps_lock", "left_control", "--dry-run"]).contains("swap"));
    assert!(e.fails(&["hotkey", "escape"]).contains("f13..f20"));
    assert!(e.fails(&["map", "windows", "nope", "a"]).contains("not a forwarded key"));
    assert_eq!(e.read("keymap.json"), "{}\n");
    e.ok(&["swap", "windows", "caps_lock", "left_control"]);
    e.ok(&["drop", "mac", "right_option"]);
    let out = e.ok(&["hotkey", "f15"]);
    assert!(out.contains("Karabiner rule must be applied again"));
    let km = e.read("keymap.json");
    assert!(km.contains("\"f15\"") && km.contains("right_option") && km.contains("caps_lock"), "{km}");
    assert!(e.ok(&["keys"]).contains("lang1"));
    // status: edited -> validated -> applied (fake installed folder with the installer's unquoted copy)
    assert!(e.ok(&["status"]).contains("config.sh: edited"));
    e.ok(&["validate"]);
    assert!(e.ok(&["status"]).contains("config.sh: validated"));
    std::fs::create_dir_all(e.installed()).unwrap();
    std::fs::write(e.installed().join("config.sh"), "KFM_PEER=192.0.2.10:47501\nKFM_BIND=192.0.2.20:47502\nKFM_FLOW_EDGE=right\nKFM_FLOW_DISPLAY=all\nKFM_LINK_TIMEOUT=\nKFM_KEYBOARD_VENDOR_ID=\nKFM_KEYBOARD_PRODUCT_ID=\nKFM_TARGET_OS=windows\n").unwrap();
    std::fs::copy(e.local().join("keymap.json"), e.installed().join("keymap.json")).unwrap();
    let st = e.ok(&["status"]);
    assert!(st.contains("config.sh: applied") && st.contains("keymap.json: applied"), "{st}");
    // restore the keymap from before the hotkey change (the newest backup holds the keymap as it was)
    let list = e.ok(&["restore", "--list"]);
    let newest = list.lines().last().unwrap().trim().to_string();
    assert!(e.read("keymap.json").contains("f15"));
    e.ok(&["restore", &newest]);
    assert!(!e.read("keymap.json").contains("f15"), "restored");
    assert!(e.fails(&["restore", "no-such-backup"]).contains("no backup"));
    e.ok(&["reset", "all"]);
    assert_eq!(e.read("keymap.json"), "{}\n");
}

#[test]
fn help_and_doctor_run_and_change_nothing() {
    let e = Env::new("doctor");
    assert!(e.ok(&["--help"]).contains("kfm-config doctor"));
    let o = e.run(&["doctor"]);
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("platform:") && text.contains("Nothing was changed"), "{text}");
    assert!(o.status.code() == Some(1), "no settings yet is a problem");
    assert!(!e.local().exists(), "doctor wrote nothing");
}

#[test]
fn windows_source_init_prints_a_keygen_command_that_works_on_every_os() {
    let e = Env::new("keygen");
    let init = e.ok(&["init", "--role", "windows-source"]);
    let key = key_arg(&e);
    assert!(init.contains("keygen") && init.contains("--out") && init.contains(&key), "init prints the keygen command with the default path:\n{init}");
    assert!(!init.contains("kfm-mac"), "kfm-mac cannot run on Windows:\n{init}");
    assert!(!Path::new(&key).exists(), "init itself makes no key");
    // the same command the plan prints for a missing key
    let o = e.run(&["keygen", "--out", &key, "--dry-run"]);
    assert!(o.status.success() && !Path::new(&key).exists(), "dry run writes nothing");
    let o = e.run(&["keygen", "--out", &key]);
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(o.status.success(), "{out}{err}");
    let bytes = std::fs::read(&key).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(text.trim().len(), 64, "64 hex characters");
    assert!(text.trim().chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!out.contains(text.trim()) && !err.contains(text.trim()), "the key is never printed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
    }
    // a second run refuses and leaves the key alone
    let o = e.run(&["keygen", "--out", &key]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("refusing to overwrite"));
    assert_eq!(std::fs::read_to_string(&key).unwrap(), text);
}

/// `kfm-karabiner preflight` must report a keyboard mismatch as a problem, so install.sh (which greps `problems: []`) fails closed.
#[test]
fn preflight_fails_when_only_the_keyboard_id_changed() {
    let e = Env::new("device");
    let dir = e.root.join("karabiner dir");
    std::fs::create_dir_all(&dir).unwrap();
    let (base, installed) = (dir.join("base.json"), dir.join("installed.json"));
    std::fs::write(&base, r#"{"profiles":[{"name":"p","selected":true,"complex_modifications":{"rules":[]}}]}"#).unwrap();
    let kar = |config: &Path, vendor: Option<&str>, args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_kfm-karabiner"));
        c.args(args).arg("--config").arg(config).env_remove("KFM_KEYMAP").env_remove("KFM_KEYBOARD_VENDOR_ID").env_remove("KFM_KEYBOARD_PRODUCT_ID");
        if let Some(v) = vendor { c.env("KFM_KEYBOARD_VENDOR_ID", v).env("KFM_KEYBOARD_PRODUCT_ID", "0x5678"); }
        c.output().unwrap()
    };
    let o = Command::new(env!("CARGO_BIN_EXE_kfm-karabiner")).args(["plan", "--config"]).arg(&base).arg("--out").arg(&installed)
        .env_remove("KFM_KEYMAP").env_remove("KFM_KEYBOARD_VENDOR_ID").env_remove("KFM_KEYBOARD_PRODUCT_ID").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let same = kar(&installed, None, &["preflight"]);
    let text = String::from_utf8_lossy(&same.stdout).into_owned();
    assert!(same.status.success() && text.contains("problems: []") && text.contains("device: configured 0x320f:0x5055, installed rule 0x320f:0x5055"), "{text}");
    let other = kar(&installed, Some("0x1234"), &["preflight"]);
    let text = String::from_utf8_lossy(&other.stdout).into_owned();
    assert!(!other.status.success() && !text.contains("problems: []") && text.contains("device: configured 0x1234:0x5678, installed rule 0x320f:0x5055"), "{text}");
}
