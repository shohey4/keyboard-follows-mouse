//! Settings tool for people and for AI assistants: diagnose, create and edit
//! `local/config.sh` + `local/keymap.json`, then print (never run) the install
//! steps. Works on macOS and Windows. It never installs anything, restarts a
//! resident, changes Karabiner, permissions or the firewall, and never reads
//! the key file. The logic lives in `keyboard_follows_mouse::settings`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use keyboard_follows_mouse::flow::{outer_edge_share, Edge, Rect};
use keyboard_follows_mouse::remap::{self, TargetOs};
use keyboard_follows_mouse::settings::*;

const HELP: &str = "\
kfm-config: check, create and edit keyboard-follows-mouse settings (it never installs or restarts anything)

Settings live in <repo>/local/config.sh (hosts, layout) and local/keymap.json (hotkey, remaps).
Every change is validated as a whole, backed up to local/backups/<timestamp>/ and written atomically.
Add --dry-run to any change to see before -> after and the validation result without writing.

  kfm-config doctor                       read-only diagnosis (each problem: cause / next / undo); exit 0 = no problem
  kfm-config init --role ROLE             create local/config.sh and an empty keymap; never overwrites
                                          ROLE = mac-source | receiver | windows-source (experimental)
  kfm-config show [--reveal]              the settings in plain words (addresses and paths masked unless --reveal)
  kfm-config set KEY VALUE [KEY VALUE..]  KEY = KFM_ROLE KFM_KEY KFM_PEER KFM_BIND KFM_FLOW_EDGE KFM_FLOW_DISPLAY
                                          KFM_LINK_TIMEOUT KFM_TARGET_OS KFM_KEYMAP KFM_KEYBOARD_VENDOR_ID/PRODUCT_ID
  kfm-config map TARGET FROM TO           TARGET windows|mac: key FROM sends key TO (a name from `keys`) on that receiver
  kfm-config drop TARGET KEY              KEY does nothing on that receiver
  kfm-config swap TARGET A B              exchange two keys (a one-way map onto an existing key is refused)
  kfm-config unmap TARGET KEY             remove one mapping
  kfm-config hotkey KEY                   switch hotkey, f13..f20 only (then the Karabiner rule must be applied again)
  kfm-config reset windows|mac|all        defaults for one target, or hotkey + all remaps
  kfm-config keys                         the forwarded key names (left/right modifiers and JIS lang1/lang2 included)
  kfm-config layout displays              this computer's displays and which edges are OUTER edges
  kfm-config validate                     validate both files; on success record a fingerprint in local/.validated
  kfm-config status                       per file: edited | validated | applied | restart needed; Karabiner re-apply?
  kfm-config restore --list | TIMESTAMP   list backups / restore one (the current files are backed up first)
  kfm-config apply                        print the exact plan, dry runs and rollbacks for THIS computer; runs nothing
  kfm-config run-args receiver|windows-source [--shell sh|powershell]
                                          kfm-receive / kfm-send arguments from config.sh, quoted for this OS

Options: --dir PATH (settings folder, default <repo>/local), --installed-dir PATH (tests), --dry-run, --reveal, --help.
The repo is found from this binary's location or the current folder; no .git is needed.";

struct Args { pos: Vec<String>, dir: Option<PathBuf>, installed: Option<PathBuf>, dry_run: bool, reveal: bool, list: bool, role: Option<String>, shell: Option<String> }

fn parse_args(raw: &[String]) -> Result<Args, String> {
    let mut a = Args { pos: vec![], dir: None, installed: None, dry_run: false, reveal: false, list: false, role: None, shell: None };
    let mut it = raw.iter();
    while let Some(x) = it.next() {
        let mut val = |n: &str| it.next().cloned().ok_or(format!("{n} needs a value"));
        match x.as_str() {
            "--dir" => a.dir = Some(PathBuf::from(val("--dir")?)),
            "--installed-dir" => a.installed = Some(PathBuf::from(val("--installed-dir")?)),
            "--role" => a.role = Some(val("--role")?),
            "--shell" => a.shell = Some(val("--shell")?),
            "--dry-run" => a.dry_run = true,
            "--reveal" => a.reveal = true,
            "--list" => a.list = true,
            "--help" | "-h" => a.pos.insert(0, "help".into()),
            o if o.starts_with("--") => return Err(format!("unknown option {o} (see --help)")),
            _ => a.pos.push(x.clone()),
        }
    }
    Ok(a)
}

/// The repo root: the folder with Cargo.toml and examples/config.example.sh above this binary or the current folder.
fn find_root() -> Option<PathBuf> {
    let is_root = |p: &Path| p.join("Cargo.toml").is_file() && p.join("examples/config.example.sh").is_file();
    let starts = [std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)), std::env::current_dir().ok()];
    starts.into_iter().flatten().find_map(|s| s.ancestors().find(|a| is_root(a)).map(Path::to_path_buf))
}

struct Ctx { root: Option<PathBuf>, ws: Workspace }

impl Ctx {
    fn new(a: &Args) -> Result<Ctx, String> {
        let root = find_root();
        let dir = match (&a.dir, &root) {
            (Some(d), _) => std::path::absolute(d).map_err(|e| format!("{}: {e}", d.display()))?,
            (None, Some(r)) => r.join("local"),
            (None, None) => return Err("cannot find the repository (run this inside the downloaded folder, or pass --dir PATH)".into()),
        };
        Ok(Ctx { root, ws: Workspace { dir, installed: a.installed.clone().or_else(default_installed_dir) } })
    }
    fn root(&self) -> Result<&Path, String> { self.root.as_deref().ok_or_else(|| "cannot find the repository root (Cargo.toml + examples/); run from inside the downloaded folder".into()) }
    fn release_dir(&self) -> Option<PathBuf> { self.root.as_ref().map(|r| r.join("target/release")) }
    fn exe(&self, name: &str) -> Option<PathBuf> {
        let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
        let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join(&file)));
        [beside, self.release_dir().map(|d| d.join(&file))].into_iter().flatten().find(|p| p.is_file())
    }
    fn settings(&self) -> Option<Settings> { self.ws.read_config().ok().flatten().and_then(|t| Settings::parse(&t).ok()) }

    /// This computer's displays, from `kfm-receive --list-displays` (None when it is not built).
    fn displays(&self) -> Option<Vec<Rect>> {
        let exe = self.exe("kfm-receive")?;
        let out = Command::new(exe).arg("--list-displays").output().ok().filter(|o| o.status.success())?;
        Some(parse_display_list(&String::from_utf8_lossy(&out.stdout)))
    }

    /// `kfm-karabiner preflight` with the same keymap and keyboard ids the rule would be applied with.
    fn preflight(&self, s: &Settings) -> Option<Result<String, String>> {
        if !cfg!(target_os = "macos") { return None; }
        let exe = self.exe("kfm-karabiner")?;
        let config = Path::new(&home_dir()?).join(".config/karabiner/karabiner.json");
        if !config.is_file() { return None; }
        let mut c = Command::new(exe);
        c.arg("preflight").arg("--config").arg(&config).env_remove("KFM_KEYMAP").env_remove("KFM_KEYBOARD_VENDOR_ID").env_remove("KFM_KEYBOARD_PRODUCT_ID");
        if self.ws.keymap_path().is_file() { c.env("KFM_KEYMAP", self.ws.keymap_path()); }
        if !s.get("KFM_KEYBOARD_VENDOR_ID").is_empty() { c.env("KFM_KEYBOARD_VENDOR_ID", s.get("KFM_KEYBOARD_VENDOR_ID")).env("KFM_KEYBOARD_PRODUCT_ID", s.get("KFM_KEYBOARD_PRODUCT_ID")); }
        let o = c.output().ok()?;
        let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        Some(if o.status.success() { Ok(text) } else { Err(text) })
    }
}

#[cfg(target_os = "macos")]
fn launchd_jobs() -> Option<Jobs> {
    const OURS: [&str; 2] = ["local.kfm-mac", "local.kfm-guard"];
    let uid = unsafe { libc::getuid() };
    let listed = Command::new("launchctl").arg("list").output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let labels: Vec<&str> = listed.lines().filter_map(|l| l.split_whitespace().nth(2)).filter(|l| { let l = l.to_lowercase(); l.contains("r75") || l.contains("kfm") }).collect();
    let mut jobs = Jobs::default();
    for label in OURS {
        let printed = Command::new("launchctl").args(["print", &format!("gui/{uid}/{label}")]).output().is_ok_and(|o| o.status.success());
        if printed || labels.contains(&label) { jobs.ours_loaded.push(label.to_string()); }
    }
    jobs.others = labels.into_iter().filter(|l| !OURS.contains(l)).map(String::from).collect();
    Some(jobs)
}
#[cfg(not(target_os = "macos"))]
fn launchd_jobs() -> Option<Jobs> { None }

/// How long the running kfm-mac has been up, from `ps -o etime=` (macOS).
#[cfg(target_os = "macos")]
fn kfm_mac_running() -> Running {
    let Ok(p) = Command::new("pgrep").args(["-x", "kfm-mac"]).output() else { return Running::Unknown };
    let text = String::from_utf8_lossy(&p.stdout);
    let Some(pid) = text.split_whitespace().next() else { return if p.status.code() == Some(1) { Running::NotRunning } else { Running::Unknown } };
    let Ok(ps) = Command::new("ps").args(["-o", "etime=", "-p", pid]).output() else { return Running::Unknown };
    match parse_etime(&String::from_utf8_lossy(&ps.stdout)) {
        Some(secs) => Running::Since(std::time::SystemTime::now() - std::time::Duration::from_secs(secs)),
        None => Running::Unknown,
    }
}
#[cfg(not(target_os = "macos"))]
fn kfm_mac_running() -> Running { Running::Unknown }

fn facts(c: &Ctx, s: Option<&Settings>) -> Facts {
    let os = std::env::consts::OS;
    let names: &[&str] = match os { "macos" => &["kfm-mac", "kfm-karabiner", "kfm-receive", "kfm-config"], _ => &["kfm-receive", "kfm-send", "kfm-config"] };
    let rel = c.release_dir();
    let built = |n: &str| rel.as_ref().is_some_and(|d| d.join(if cfg!(windows) { format!("{n}.exe") } else { n.to_string() }).is_file());
    let karabiner = os == "macos" && (Path::new("/Applications/Karabiner-Elements.app").exists() || Path::new("/Library/Application Support/org.pqrs/Karabiner-Elements").exists());
    let default = Settings::default();
    Facts {
        os, arch: std::env::consts::ARCH,
        binaries: names.iter().map(|n| (n.to_string(), built(n))).collect(),
        karabiner_installed: (os == "macos").then_some(karabiner),
        preflight: if s.is_some_and(|s| s.role() == Some(Role::MacSource)) { c.preflight(s.unwrap_or(&default)) } else { None },
        jobs: if os == "macos" { launchd_jobs() } else { None },
        displays: c.displays(),
        repo_root: c.root.clone().unwrap_or_default(),
    }
}

fn need<'a>(a: &'a Args, n: usize, usage: &str) -> Result<&'a [String], String> {
    if a.pos.len() == n + 1 { Ok(&a.pos[1..]) } else { Err(format!("usage: kfm-config {usage}")) }
}

fn target(s: &str) -> Result<TargetOs, String> { TargetOs::parse(s).map_err(|_| format!("target must be windows or mac (the receiver's OS), got {s:?}")) }

/// Show a plan, then write it unless `--dry-run`. Returns the exit code.
fn finish(c: &Ctx, a: &Args, plan: Plan) -> Result<ExitCode, String> {
    println!("{}", plan.preview(a.reveal));
    if !plan.report.writable() { println!("\nnothing was written: the result would be invalid."); return Ok(ExitCode::FAILURE); }
    if a.dry_run { println!("\ndry run: nothing was written. Run again without --dry-run to apply this change."); return Ok(ExitCode::SUCCESS); }
    if !plan.changed() { println!("\nnothing to write."); return Ok(ExitCode::SUCCESS); }
    match plan.commit(&c.ws)? {
        Some(b) => println!("\nwritten. previous files backed up to {} (undo: kfm-config restore {})", b.display(), b.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())),
        None => println!("\nwritten (there was nothing to back up)."),
    }
    println!("next: `kfm-config validate`, then `kfm-config status` / `kfm-config apply`.");
    Ok(ExitCode::SUCCESS)
}

fn key_op(c: &Ctx, a: &Args, op: KeyOp) -> Result<ExitCode, String> { finish(c, a, Plan::key_edit(&c.ws, &op, c.displays().as_deref())?) }

fn cmd_doctor(c: &Ctx) -> Result<ExitCode, String> {
    let s = c.settings();
    let f = facts(c, s.as_ref());
    println!("kfm-config doctor (read-only)\nsettings folder: {}\nrepository: {}", c.ws.dir.display(), c.root.as_ref().map_or("(not found)".into(), |r| r.display().to_string()));
    let mut checks = doctor_checks(&c.ws, &f);
    if f.os != "macos" && f.os != "windows" { checks.insert(1, Check { level: Level::Problem, title: format!("{} is not supported", f.os), detail: String::new(), cause: "only macOS and Windows are supported".into(), next: "use a Mac or a Windows PC".into(), undo: "none".into() }); }
    let problems = checks.iter().filter(|k| k.level == Level::Problem).count();
    for k in &checks { println!("{}", k.render()); }
    println!("\n{problems} problem(s), {} warning(s). Nothing was changed.", checks.iter().filter(|k| k.level == Level::Warn).count());
    Ok(if problems == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn cmd_init(c: &Ctx, a: &Args) -> Result<ExitCode, String> {
    let role = Role::parse(a.role.as_deref().ok_or("usage: kfm-config init --role mac-source|receiver|windows-source")?)?;
    if a.dry_run {
        println!("dry run: would create {} (role {}) and {} (empty keymap); nothing written.\n{}", c.ws.config_path().display(), role.as_str(), c.ws.keymap_path().display(), if c.ws.config_path().exists() { "but config.sh already exists: init would refuse" } else { "" });
        return Ok(ExitCode::SUCCESS);
    }
    let made = init(&c.ws, role)?;
    for p in &made { println!("created {}", p.display()); }
    println!("next: set the addresses and layout, e.g. `kfm-config set KFM_PEER <other computer IP:PORT> KFM_BIND <this computer IP:PORT>`, then `kfm-config validate`.");
    let key = c.ws.dir.join("kfm-psk.key").to_string_lossy().into_owned();
    if role == Role::Receiver { println!("the key file is NOT made here: copy the file made on the source computer to exactly this path yourself (USB, AirDrop; never through chat):\n  {key}"); }
    else {
        let kfm_mac = c.root().map(|r| r.join("target/release").join(if cfg!(windows) { "kfm-mac.exe" } else { "kfm-mac" }).to_string_lossy().into_owned()).unwrap_or_else(|_| "kfm-mac".into());
        println!("the key file (default KFM_KEY) does not exist yet. kfm-config does not make it; to make it on this computer (needs your approval, creates one new file, mode 600):\n  {}\nthen copy that file to the receiver yourself (USB, AirDrop; never through chat).", Shell::native().command(&kfm_mac, &["keygen".into(), "--out".into(), key]));
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_show(c: &Ctx, a: &Args) -> Result<ExitCode, String> {
    match c.ws.read_config()? {
        None => println!("no settings yet ({} does not exist): run `kfm-config init --role ...`", c.ws.config_path().display()),
        Some(t) => {
            let s = Settings::parse(&t).map_err(|e| format!("{}: {e}", c.ws.config_path().display()))?;
            let same = Path::new(s.get("KFM_KEYMAP")) == c.ws.keymap_path();
            println!("{}", describe_settings(&s, a.reveal, same));
        }
    }
    let k = remap::parse(c.ws.read_keymap()?.as_deref().unwrap_or("{}")).map_err(|e| format!("keymap.json: {e}"))?;
    println!("\nKeys:\n  {}", keymap_lines(&k).join("\n  "));
    if !a.reveal { println!("\n(addresses and paths are masked; --reveal shows them. The key file is never printed.)"); }
    Ok(ExitCode::SUCCESS)
}

fn cmd_keys() -> Result<ExitCode, String> {
    let names: Vec<&str> = remap::forwarded_names().collect();
    println!("Forwarded keys ({}), usable as FROM and TO (TO may also be `drop`):", names.len());
    for chunk in names.chunks(8) { println!("  {}", chunk.join(" ")); }
    println!("Left/right modifiers are separate keys (left_control, right_option, ...); lang1 / lang2 are the JIS Kana / Eisu keys.\nThe switch hotkey is one of: {} (not forwarded, not remappable).", remap::EXTRA_HOTKEYS.join(" "));
    Ok(ExitCode::SUCCESS)
}

fn cmd_layout(c: &Ctx, a: &Args) -> Result<ExitCode, String> {
    need(a, 1, "layout displays")?;
    if a.pos[1] != "displays" { return Err("usage: kfm-config layout displays".into()); }
    let ds = c.displays().ok_or("cannot list displays: kfm-receive is not built (run the setup script or `cargo build --release`)")?;
    println!("This computer has {} display(s). Numbers are what KFM_FLOW_DISPLAY uses (1-based); an OUTER edge has no other display beyond it.", ds.len());
    for (i, r) in ds.iter().enumerate() {
        let shares: Vec<String> = [("left", Edge::Left), ("right", Edge::Right), ("top", Edge::Top), ("bottom", Edge::Bottom)].iter().map(|(n, e)| match outer_edge_share(&ds, i, *e) { 100 => format!("{n}: outer"), 0 => format!("{n}: shared"), p => format!("{n}: {p}% outer") }).collect();
        println!("  display {}: {}x{} at ({}, {})  {}", i + 1, r.w, r.h, r.x, r.y, shares.join(", "));
    }
    if let Some(s) = c.settings() {
        println!("current setting: edge {}, display {}", if s.get("KFM_FLOW_EDGE").is_empty() { "(default)" } else { s.get("KFM_FLOW_EDGE") }, if s.get("KFM_FLOW_DISPLAY").is_empty() { "all" } else { s.get("KFM_FLOW_DISPLAY") });
        for w in layout_warnings(&s, &ds) { println!("  warning: {w}"); }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_validate(c: &Ctx) -> Result<ExitCode, String> {
    let r = validate(&c.ws, c.displays().as_deref())?;
    println!("{}", r.summary());
    if r.complete() { println!("validated; fingerprint recorded in {}", c.ws.validated_path().display()); Ok(ExitCode::SUCCESS) } else { println!("not validated."); Ok(ExitCode::FAILURE) }
}

fn cmd_status(c: &Ctx) -> Result<ExitCode, String> {
    if let Some(j) = launchd_jobs() && !j.others.is_empty() {
        println!("WARNING: an existing installation is loaded in launchd ({}). Do not overwrite it; ask the user. \"Not applied / not installed\" below only describes THIS project's installed copies, not what is on this Mac.", j.others.join(", "));
    }
    let st = status(&c.ws, kfm_mac_running())?;
    for f in &st {
        let word = match f.state { State::Missing => "missing", State::Edited => "edited", State::Validated => "validated", State::Applied => "applied" };
        println!("{}: {word} - {}", f.file, f.note);
    }
    match &c.ws.installed { Some(d) => println!("installed copies are read from {}", d.display()), None => println!("(no installed-copy comparison on this OS: nothing is installed by install.sh here)") }
    if let Some(s) = c.settings() && s.role() == Some(Role::MacSource) {
        match c.preflight(&s).map(|r| r.unwrap_or_else(|e| e)).map(|t| rule_state(&t)) {
            Some(RuleState::HotkeyDiffers { configured, installed }) => println!("Karabiner: the installed rule uses {installed} but the keymap says {configured}: apply the rule again (`kfm-config apply` shows the command)."),
            Some(RuleState::NotInstalled) => println!("Karabiner: the kfm rule is not installed yet (`kfm-config apply`)."),
            Some(RuleState::Current) => println!("Karabiner: the installed rule matches the configured hotkey."),
            _ => println!("Karabiner: not checked (kfm-karabiner not built, or no karabiner.json)."),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_restore(c: &Ctx, a: &Args) -> Result<ExitCode, String> {
    if a.list || a.pos.len() == 1 {
        let l = list_backups(&c.ws);
        if l.is_empty() { println!("no backups yet ({})", c.ws.backups_dir().display()); } else { println!("backups in {} (newest last):", c.ws.backups_dir().display()); for b in l { println!("  {b}"); } }
        return Ok(ExitCode::SUCCESS);
    }
    let stamp = &need(a, 1, "restore --list | TIMESTAMP")?[0];
    if !list_backups(&c.ws).contains(stamp) { return Err(format!("no backup {stamp:?} (see `kfm-config restore --list`)")); }
    if a.dry_run { println!("dry run: would restore the files in backup {stamp} (backing up the current ones first); nothing written."); return Ok(ExitCode::SUCCESS); }
    let (backup, done) = restore(&c.ws, stamp)?;
    println!("restored {} from backup {stamp}.", done.join(" and "));
    if let Some(b) = backup { println!("the files before the restore are in {} (undo: kfm-config restore {})", b.display(), b.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())); }
    println!("next: `kfm-config validate`, `kfm-config status`.");
    Ok(ExitCode::SUCCESS)
}

fn cmd_apply(c: &Ctx) -> Result<ExitCode, String> {
    let root = c.root()?;
    let jobs = launchd_jobs();
    if let Some(m) = jobs.as_ref().and_then(foreign_stop_message) { println!("{m}"); return Ok(ExitCode::FAILURE); }
    let s = c.settings().ok_or_else(|| format!("{} is missing or does not parse: run `kfm-config doctor`", c.ws.config_path().display()))?;
    let st = status(&c.ws, Running::Unknown)?;
    let validated = st.iter().all(|f| matches!(f.state, State::Validated | State::Applied) || (f.file == "keymap.json" && f.state == State::Missing));
    let rule = c.preflight(&s).map_or(RuleState::Unknown, |r| rule_state(&r.unwrap_or_else(|e| e)));
    let key_exists = !matches!(key_file_state(s.get("KFM_KEY")), KeyFile::Missing);
    let ctx = PlanCtx { settings: &s, ws: &c.ws, repo_root: root, shell: Shell::native(), os: std::env::consts::OS, rule, jobs: jobs.as_ref(), key_exists, validated };
    println!("{}", apply_plan(&ctx)?);
    Ok(ExitCode::SUCCESS)
}

fn cmd_run_args(c: &Ctx, a: &Args) -> Result<ExitCode, String> {
    let which = match need(a, 1, "run-args receiver|windows-source [--shell sh|powershell]")?[0].as_str() { "receiver" => Role::Receiver, "windows-source" => Role::WindowsSource, o => return Err(format!("run-args needs receiver or windows-source, got {o:?}")) };
    let shell = match a.shell.as_deref() { None => Shell::native(), Some("sh") => Shell::Sh, Some("powershell") => Shell::PowerShell, Some(o) => return Err(format!("--shell must be sh or powershell, got {o:?}")) };
    let s = Settings::parse(&c.ws.read_config()?.ok_or("config.sh does not exist: run `kfm-config init` first")?)?;
    println!("{}", shell.join(&run_args(&s, which)?));
    Ok(ExitCode::SUCCESS)
}

fn run(raw: &[String]) -> Result<ExitCode, String> {
    let a = parse_args(raw)?;
    let cmd = a.pos.first().map(String::as_str).unwrap_or("help");
    if cmd == "help" { println!("{HELP}"); return Ok(ExitCode::SUCCESS); }
    let c = Ctx::new(&a)?;
    match cmd {
        "doctor" => cmd_doctor(&c),
        "init" => cmd_init(&c, &a),
        "show" => cmd_show(&c, &a),
        "set" => {
            let kv = &a.pos[1..];
            if kv.is_empty() || kv.len() % 2 != 0 { return Err("usage: kfm-config set KEY VALUE [KEY VALUE ...]".into()); }
            let pairs: Vec<(String, String)> = kv.chunks(2).map(|p| (p[0].clone(), p[1].clone())).collect();
            finish(&c, &a, Plan::set_values(&c.ws, &pairs, c.displays().as_deref())?)
        }
        "map" => { let v = need(&a, 3, "map TARGET FROM TO")?; key_op(&c, &a, KeyOp::Map { target: target(&v[0])?, from: v[1].clone(), to: v[2].clone() }) }
        "drop" => { let v = need(&a, 2, "drop TARGET KEY")?; key_op(&c, &a, KeyOp::Drop { target: target(&v[0])?, from: v[1].clone() }) }
        "unmap" => { let v = need(&a, 2, "unmap TARGET KEY")?; key_op(&c, &a, KeyOp::Unmap { target: target(&v[0])?, from: v[1].clone() }) }
        "swap" => { let v = need(&a, 3, "swap TARGET A B")?; key_op(&c, &a, KeyOp::Swap { target: target(&v[0])?, a: v[1].clone(), b: v[2].clone() }) }
        "hotkey" => key_op(&c, &a, KeyOp::Hotkey(need(&a, 1, "hotkey KEY")?[0].clone())),
        "reset" => key_op(&c, &a, match need(&a, 1, "reset windows|mac|all")?[0].as_str() { "all" => KeyOp::ResetAll, t => KeyOp::ResetTarget(target(t)?) }),
        "keys" => cmd_keys(),
        "layout" => cmd_layout(&c, &a),
        "validate" => cmd_validate(&c),
        "status" => cmd_status(&c),
        "restore" => cmd_restore(&c, &a),
        "apply" => cmd_apply(&c),
        "run-args" => cmd_run_args(&c, &a),
        o => Err(format!("unknown command {o:?} (see `kfm-config --help`)")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => { eprintln!("kfm-config: {e}"); ExitCode::FAILURE }
    }
}
