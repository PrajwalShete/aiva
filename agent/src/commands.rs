//! The smaller CLI commands: status, folders, service control, logs, doctor, update...

use crate::config::{self, Config};
use crate::service::{self, State};
use crate::setup::{get_json, http};
use crate::ui::{self, bold, cyan, dim};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// The server reports whether it sees this computer, to whoever holds its key.
pub fn server_status(cfg: &Config) -> Result<serde_json::Value> {
    let url = format!("{}/api/status", cfg.server.trim_end_matches('/'));
    let mut req = http().get(&url);
    if let Some(key) = &cfg.key {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    Ok(req.call()?.body_mut().read_json()?)
}

pub fn status(as_json: bool) -> Result<()> {
    let cfg = Config::require()?;
    let state = service::state()?;
    let remote = server_status(&cfg);
    if as_json {
        let out = json!({
            "version": crate::VERSION,
            "id": cfg.id(),
            "url": cfg.connector_url(),
            "service": state.to_string(),
            "online": remote.as_ref().ok().and_then(|v| v["online"].as_bool()),
            "folders": cfg.folders.iter().map(|f| json!({"name": f.label, "path": f.path, "access": f.access})).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let online = remote.as_ref().ok().and_then(|v| v["online"].as_bool()) == Some(true);
    let dot = if online { console::style("●").green().to_string() } else { console::style("●").red().to_string() };
    println!();
    println!("  {}  {}  {}", console::style("●").color256(208), bold("aiva"), dim(format!("v{}", crate::VERSION)));
    println!();
    row(
        "Status",
        match &remote {
            Ok(v) if online => format!("{dot} Online{}", v["device"].as_str().map(|d| dim(format!(" as {d}"))).unwrap_or_default()),
            Ok(_) => format!("{dot} Offline {}", dim("· your AI can't reach this computer (see aiva doctor)")),
            Err(e) => format!("{} Unknown {}", console::style("●").yellow(), dim(format!("· {e}"))),
        },
    );
    row("URL", cyan(cfg.connector_url()));
    row(
        "Service",
        match state {
            service::State::Running { .. } => format!("Running {}", dim("· starts at login")),
            service::State::Stopped => format!("Stopped {}", dim("· aiva start")),
            service::State::NotInstalled => format!("Not installed {}", dim("· aiva service install")),
        },
    );
    row("Folders", format!("{} shared", cfg.folders.len()));
    let w = cfg.folders.iter().map(|f| f.label.chars().count()).max().unwrap_or(0).max(8);
    for f in &cfg.folders {
        let access = if f.writable() { ui::accent(format!("{:<12}", "read + write")) } else { dim(format!("{:<12}", "read-only")) };
        println!("  {:<12}{:<w$}  {access}  {}", "", f.label, dim(ui::tilde(&f.path)));
    }
    println!();
    Ok(())
}

fn row(label: &str, value: impl std::fmt::Display) {
    println!("  {}{value}", dim(format!("{label:<12}")));
}

pub fn folders_list() -> Result<()> {
    let cfg = Config::load_or_default()?;
    if cfg.folders.is_empty() {
        ui::info(format!("No folders shared yet. Add one with {}", cyan("aiva folders add <path>")));
        return Ok(());
    }
    for f in &cfg.folders {
        let access = if f.writable() { "read + write" } else { "read-only" };
        println!("  {:<18} {}  {}", bold(&f.label), ui::tilde(&f.path), dim(access));
    }
    ui::info(dim("Claude refers to files as <name>/<path>, e.g. ".to_string() + &cfg.folders[0].label + "/README.md"));
    Ok(())
}

pub fn folders_add(paths: &[PathBuf], write: bool) -> Result<()> {
    let mut cfg = Config::load_or_default()?;
    for path in paths {
        let label = match cfg.add_folder(path) {
            Ok(f) => f.label.clone(),
            Err(e) => {
                ui::fail(format!("{e:#}"));
                continue;
            }
        };
        let f = cfg.folder_mut(&label).unwrap();
        if write {
            f.access = "write".into();
        }
        let access = if f.writable() { "read + write" } else { "read-only" };
        ui::ok(format!("Sharing {}  {}  {}", bold(&f.label), dim(ui::tilde(&f.path)), access));
    }
    cfg.save()?;
    if write {
        edits_note();
    }
    ui::info(dim("Takes effect within a few seconds; no restart needed."));
    Ok(())
}

pub fn folders_set(target: &str, access: &str) -> Result<()> {
    let mut cfg = Config::load_or_default()?;
    let Some(f) = cfg.folder_mut(target) else { bail!("No shared folder called {target}. See `aiva folders`.") };
    f.access = access.to_string();
    let label = f.label.clone();
    cfg.save()?;
    if access == "write" {
        ui::ok(format!("Claude can now create and edit files in {}", bold(&label)));
        edits_note();
    } else {
        ui::ok(format!("{} is read-only again", bold(&label)));
    }
    ui::info(dim("Takes effect within a few seconds. Start a new Claude chat to see the edit tools appear or disappear."));
    Ok(())
}

fn edits_note() {
    ui::info(dim("Every change is backed up: see `aiva history`, revert with `aiva undo` (or ask Claude to undo)."));
}

pub fn history(lines: usize) -> Result<()> {
    let changes = crate::tools::changes::load();
    if changes.is_empty() {
        ui::info("No changes yet.");
        return Ok(());
    }
    for c in changes.iter().rev().take(lines) {
        let when = crate::tools::ago(std::time::UNIX_EPOCH + std::time::Duration::from_secs(c.time));
        let files: Vec<_> = c.files.iter().map(|f| f.display.as_str()).collect();
        let undone = if c.undone { dim(" (undone)") } else { String::new() };
        println!("  {} {:<9} {:<11} {}{undone}", bold(format!("#{:<4}", c.id)), dim(when), c.tool, files.join(", "));
        if !c.summary.is_empty() {
            println!("        {}", dim(&c.summary));
        }
    }
    Ok(())
}

pub fn undo(id: Option<u64>) -> Result<()> {
    let change = crate::tools::changes::undo(id, None).map_err(|e| anyhow::anyhow!(e.0))?;
    let files: Vec<_> = change.files.iter().map(|f| f.display.as_str()).collect();
    ui::ok(format!("Reverted change #{} ({}): {}", change.id, change.tool, files.join(", ")));
    Ok(())
}

pub fn folders_remove(targets: &[String]) -> Result<()> {
    let mut cfg = Config::load_or_default()?;
    for target in targets {
        match cfg.remove_folder(target) {
            Some(f) => ui::ok(format!("Stopped sharing {}  {}", bold(&f.label), dim(ui::tilde(&f.path)))),
            None => ui::fail(format!("No shared folder called {target}. See `aiva folders`.")),
        }
    }
    cfg.save()
}

pub fn service_install() -> Result<()> {
    service::install(&service::current_exe()?)?;
    ui::ok(format!("Installed and started ({})", service::describe()));
    Ok(())
}

pub fn service_uninstall() -> Result<()> {
    service::uninstall()?;
    ui::ok("Service removed. Claude can't reach this computer until you run `aiva service install`.");
    Ok(())
}

pub fn service_status() -> Result<()> {
    println!("{}  {}", service::state()?, dim(service::describe()));
    Ok(())
}

pub fn start() -> Result<()> {
    match service::state()? {
        State::NotInstalled => service_install(),
        State::Running { .. } => {
            ui::ok("Already running");
            Ok(())
        }
        State::Stopped => {
            service::start()?;
            ui::ok("Started");
            Ok(())
        }
    }
}

pub fn stop() -> Result<()> {
    service::stop()?;
    ui::ok("Stopped. Claude can't reach this computer until you run `aiva start`.");
    Ok(())
}

pub fn restart() -> Result<()> {
    service::restart()?;
    ui::ok("Restarted");
    Ok(())
}

pub fn logs(lines: usize, follow: bool) -> Result<()> {
    let path = config::log_path();
    let mut file = std::fs::File::open(&path).with_context(|| format!("no log yet at {}", path.display()))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let all: Vec<&str> = text.lines().collect();
    for line in &all[all.len().saturating_sub(lines)..] {
        println!("{line}");
    }
    if !follow {
        return Ok(());
    }
    let mut pos = file.stream_position()?;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let len = std::fs::metadata(&path)?.len();
        if len < pos {
            pos = 0; // log was truncated
        }
        if len > pos {
            file.seek(SeekFrom::Start(pos))?;
            let mut chunk = String::new();
            file.read_to_string(&mut chunk)?;
            print!("{chunk}");
            std::io::stdout().flush()?;
            pos = len;
        }
    }
}

pub fn doctor() -> Result<()> {
    ui::heading("aiva doctor");
    let mut problems = 0;
    let mut check = |ok: bool, good: String, bad: String| {
        if ok {
            ui::ok(good);
        } else {
            problems += 1;
            ui::fail(bad);
        }
    };

    let cfg = match Config::load()? {
        Some(cfg) => cfg,
        None => {
            ui::fail("Not set up. Run `aiva setup`.");
            return Ok(());
        }
    };
    check(cfg.key.is_some(), "Key present (see it with `aiva token`)".into(), "No key. Run `aiva setup`.".into());
    check(!cfg.folders.is_empty(), format!("{} folder(s) shared", cfg.folders.len()), "No folders shared. Run `aiva folders add <path>`.".into());
    for f in &cfg.folders {
        let readable = std::fs::read_dir(&f.path);
        let bad = match &readable {
            Err(e) if cfg!(target_os = "macos") && e.raw_os_error() == Some(1) => format!(
                "{}: macOS blocked access. Open System Settings → Privacy & Security → Full Disk Access (or Files and Folders) and allow `aiva`.",
                ui::tilde(&f.path)
            ),
            Err(e) => format!("{}: {e}", ui::tilde(&f.path)),
            Ok(_) => String::new(),
        };
        check(readable.is_ok(), format!("Can read {}", ui::tilde(&f.path)), bad);
    }

    let health = http().get(&format!("{}/health", cfg.server.trim_end_matches('/'))).call();
    check(health.is_ok(), format!("Server reachable ({})", cfg.server), format!("Can't reach {}: {}", cfg.server, health.err().map(|e| e.to_string()).unwrap_or_default()));

    let state = service::state()?;
    let running = matches!(state, State::Running { .. });
    check(running, format!("Service {state}"), format!("Service {state}. Run `aiva start`."));
    if service::linger_enabled() == Some(false) {
        check(false, String::new(), "systemd linger is off, so the service stops when you log out. Run: sudo loginctl enable-linger $USER".into());
    }

    if cfg.key.is_some() {
        let online = server_status(&cfg).ok().and_then(|v| v["online"].as_bool()).unwrap_or(false);
        check(
            online,
            "Server sees this computer online".into(),
            if running { "Server doesn't see this computer yet. Check `aiva logs`.".into() } else { "Offline because the service isn't running.".into() },
        );
    }

    if problems == 0 {
        ui::info(format!("\nEverything looks good. Connector URL: {}", cyan(cfg.connector_url())));
    } else {
        ui::info(format!("\n{problems} problem(s) found."));
    }
    Ok(())
}

pub fn url() -> Result<()> {
    println!("{}", Config::require()?.connector_url());
    Ok(())
}

pub fn tools() -> Result<()> {
    let defs = crate::tools::definitions(&Config::load_or_default()?);
    for tool in defs.as_array().into_iter().flatten() {
        let name = tool["name"].as_str().unwrap_or_default();
        let first_line = tool["description"].as_str().unwrap_or_default().split(". ").next().unwrap_or_default();
        println!("  {:<16} {}", bold(name), dim(first_line));
    }
    Ok(())
}

pub fn call(tool: &str, args: &str) -> Result<()> {
    let args: serde_json::Value = serde_json::from_str(args).context("arguments must be JSON, e.g. '{\"path\": \"Jev\"}'")?;
    let cfg = Config::load_or_default()?;
    let started = std::time::Instant::now();
    let result = crate::tools::call(tool, &args, &cfg);
    println!("{}", result.text);
    eprintln!("{}", dim(format!("{} in {}ms, {} chars", if result.is_error { "error" } else { "ok" }, started.elapsed().as_millis(), result.text.len())));
    if result.is_error {
        std::process::exit(1);
    }
    Ok(())
}

pub fn show_config() -> Result<()> {
    let path = config::config_path();
    println!("{}", dim(path.display()));
    if let Some(mut cfg) = Config::load()? {
        if let Some(key) = &cfg.key {
            cfg.key = Some(format!("{}…{}  (aiva token shows it)", &key[..key.len().min(7)], &key[key.len().saturating_sub(4)..]));
        }
        println!("{}", serde_json::to_string_pretty(&cfg)?);
    }
    Ok(())
}

/// Release asset name for this platform, matching what install.sh downloads.
pub fn asset_name() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        other => bail!("No prebuilt aiva for {other} yet."),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => bail!("No prebuilt aiva for {other} yet."),
    };
    Ok(format!("aiva-{os}-{arch}{}", std::env::consts::EXE_SUFFIX))
}

/// Downloads a release file next to `exe` (same folder, so it can be renamed into place).
fn download(url: &str, exe: &std::path::Path, name: &str) -> Result<PathBuf> {
    // Windows only runs files ending in .exe.
    let tmp = exe.with_file_name(format!("{name}.download{}", std::env::consts::EXE_SUFFIX));
    let mut resp = http().get(url).call()?;
    let mut file = std::fs::File::create(&tmp)?;
    std::io::copy(&mut resp.body_mut().as_reader(), &mut file)?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(tmp)
}

pub fn update() -> Result<()> {
    let cfg = Config::load_or_default()?;
    let server = cfg.server.trim_end_matches('/');
    let latest = get_json(&format!("{server}/api/version"))?;
    let latest = latest["version"].as_str().unwrap_or_default().to_string();
    if latest == crate::VERSION {
        ui::ok(format!("Already on the latest version ({latest})"));
        return Ok(());
    }
    ui::info(format!("Updating {} → {latest}...", crate::VERSION));
    let exe = service::current_exe()?;
    let asset = asset_name()?;
    let tmp = download(&format!("{server}/dl/{asset}"), &exe, "aiva")?;
    // Make sure the download actually runs before replacing the working binary.
    let check = std::process::Command::new(&tmp).arg("--version").output();
    if !check.as_ref().is_ok_and(|o| o.status.success()) {
        let _ = std::fs::remove_file(&tmp);
        bail!("The downloaded binary didn't run; kept the current version.");
    }
    // On Windows the background launcher ships alongside (see src/bin/aivaw.rs).
    let launcher = if cfg!(windows) {
        let name = asset.replacen("aiva-", "aivaw-", 1);
        Some(download(&format!("{server}/dl/{name}"), &exe, "aivaw")?)
    } else {
        None
    };
    crate::platform::replace_running_exe(&tmp, &exe)?;
    if let Some(new) = launcher {
        crate::platform::replace_running_exe(&new, &exe.with_file_name("aivaw.exe"))?;
    }
    ui::ok(format!("Updated to {}", String::from_utf8_lossy(&check.unwrap().stdout).trim()));
    if service::restart_if_running()? {
        ui::ok("Restarted the background service");
    }
    Ok(())
}

pub fn uninstall(keep_binary: bool, yes: bool) -> Result<()> {
    if !yes {
        let sure = dialoguer::Confirm::new()
            .with_prompt("Remove the aiva service, config and logs? Claude will lose access to this computer.")
            .default(false)
            .interact()?;
        if !sure {
            return Ok(());
        }
    }
    service::uninstall()?;
    ui::ok("Removed the background service");
    let home = config::home();
    let exe = service::current_exe().ok();
    if home.exists() {
        if keep_binary || !exe.as_ref().is_some_and(|e| e.starts_with(&home)) {
            for entry in ["config.json", "logs"] {
                let p = home.join(entry);
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
        } else {
            crate::platform::remove_install_dir(&home)?;
        }
        ui::ok(format!("Removed {}", ui::tilde(&home)));
    }
    if !keep_binary && cfg!(windows) {
        crate::platform::remove_from_user_path(&home.join("bin"));
        ui::ok("Removed aiva from your PATH");
    }
    if !keep_binary && cfg!(unix) {
        if let Some(link) = dirs::home_dir().map(|h| h.join(".local/bin/aiva")) {
            if link.symlink_metadata().is_ok() {
                std::fs::remove_file(&link)?;
                ui::ok(format!("Removed {}", ui::tilde(&link)));
            }
        }
    }
    ui::info("Also remove the aiva connector from your apps (e.g. Claude → Settings → Connectors).");
    Ok(())
}
