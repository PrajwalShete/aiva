//! `aiva setup` (guided onboarding), `aiva token` and `aiva rotate`.
//!
//! This computer generates its own key; the connector URL is derived from it. Connecting an app
//! means adding the URL as a custom connector and pasting the key (the "token") on the page that
//! opens. The server never stores either.

use crate::config::{self, Config};
use crate::ui::{self, accent, bold, cyan, dim, green};
use crate::{agent, discover, service};
use anyhow::{Result, bail};
use clap::Args;
use dialoguer::{Confirm, Input, MultiSelect};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const CLAUDE_CONNECTORS: &str = "https://claude.ai/settings/connectors";

#[derive(Args, Default)]
pub struct SetupArgs {
    /// Server to use
    #[arg(long, env = "AIVA_SERVER")]
    server: Option<String>,
    /// A folder to share; repeat for more (skips the picker)
    #[arg(long = "folder", value_name = "PATH")]
    folders: Vec<PathBuf>,
    /// Don't install the background service
    #[arg(long)]
    no_service: bool,
}

pub fn run(args: SetupArgs) -> Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut cfg = Config::load_or_default()?;
    if let Some(server) = args.server {
        cfg.server = server.trim_end_matches('/').to_string();
    }
    let total = 3;

    ui::banner("Setup");
    if cfg.key.is_some() && !cfg.folders.is_empty() {
        ui::info(dim("Already set up. Running setup again keeps your URL and token; change anything below."));
    }

    // ── 1. Folders ──
    ui::step(1, total, "Choose folders");
    if !args.folders.is_empty() {
        for path in &args.folders {
            let folder = cfg.add_folder(path)?;
            ui::ok(format!("{}  {}", bold(&folder.label), dim(ui::tilde(&folder.path))));
        }
    } else if interactive {
        pick_folders(&mut cfg)?;
    } else if cfg.folders.is_empty() {
        bail!("--folder is required when not running in a terminal");
    }

    // ── 2. Edits ──
    ui::step(2, total, "Allow edits (optional)");
    if interactive && args.folders.is_empty() {
        pick_writable(&mut cfg)?;
    } else {
        ui::info(dim("Read-only. Allow edits later with `aiva folders set <name> write`."));
    }
    if cfg.key.is_none() {
        cfg.key = Some(config::generate_key()?);
    }
    cfg.save()?;

    // ── 3. Connect ──
    ui::step(3, total, "Connect your AI app");
    if !args.no_service {
        install_service_quietly()?;
    }
    let pb = ui::spinner("Connecting to the aiva relay…");
    match crate::tokio_main(agent::verify(&cfg)) {
        Ok(_) => ui::done(pb, format!("Connected to {}", dim(&cfg.server))),
        Err(e) => ui::done_warn(pb, format!("Couldn't reach the relay yet ({e}). aiva keeps retrying in the background.")),
    }
    ui::blank();
    connect_card(&cfg);

    if interactive {
        guide(&cfg)?;
    } else {
        ui::info(dim("Add the URL as a custom connector, click Connect, and paste the token."));
    }
    Ok(())
}

// ---- Step 1: folder picker --------------------------------------------------------------

fn pick_folders(cfg: &mut Config) -> Result<()> {
    ui::info(dim("Your AI can read only these folders, and never secret files like .env or keys."));
    ui::blank();
    let pb = ui::spinner("Looking for your projects…");
    let found = discover::projects(30);
    pb.finish_and_clear();

    // Already-shared folders first (checked), then discovered projects, newest first.
    struct Row {
        path: PathBuf,
        name: String,
        kind: String,
        when: String,
    }
    let mut rows: Vec<Row> = cfg
        .folders
        .iter()
        .map(|f| Row { path: f.path.clone(), name: f.label.clone(), kind: "shared".into(), when: String::new() })
        .collect();
    for p in found {
        if !rows.iter().any(|r| r.path == p.path) {
            rows.push(Row { path: p.path, name: p.name, kind: p.kind, when: crate::tools::ago(p.modified) });
        }
    }
    let name_w = rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(0).clamp(8, 24);
    let kind_w = rows.iter().map(|r| r.kind.chars().count()).max().unwrap_or(0).clamp(4, 18);
    let mut items: Vec<String> = rows
        .iter()
        .map(|r| {
            let name: String = r.name.chars().take(name_w).collect();
            format!(
                "{:<name_w$}  {}  {}  {}",
                name,
                dim(format!("{:<kind_w$}", r.kind)),
                dim(format!("{:<8}", r.when)),
                dim(ui::tilde(&r.path))
            )
        })
        .collect();
    items.push(accent("+ Add a folder by path…"));
    let defaults: Vec<bool> = rows.iter().map(|r| cfg.folders.iter().any(|f| f.path == r.path)).chain([rows.is_empty()]).collect();

    let picked = MultiSelect::with_theme(&ui::theme())
        .with_prompt(format!("Folders to share {}", dim("· space to select, enter to confirm")))
        .items(&items)
        .defaults(&defaults)
        .max_length(14)
        .report(false)
        .interact_opt()?
        .unwrap_or_default();

    let add_by_path = picked.contains(&rows.len());
    let chosen: Vec<PathBuf> = picked.iter().filter(|&&i| i < rows.len()).map(|&i| rows[i].path.clone()).collect();
    cfg.folders.retain(|f| chosen.contains(&f.path));
    for path in &chosen {
        cfg.add_folder(path)?;
    }
    if add_by_path || cfg.folders.is_empty() {
        ask_paths(cfg)?;
    }
    for f in &cfg.folders {
        ui::ok(format!("{}  {}", bold(&f.label), dim(ui::tilde(&f.path))));
    }
    Ok(())
}

fn ask_paths(cfg: &mut Config) -> Result<()> {
    loop {
        let answer: String = Input::with_theme(&ui::theme())
            .with_prompt("Folder path")
            .allow_empty(!cfg.folders.is_empty())
            .report(false)
            .interact_text()?;
        if answer.trim().is_empty() {
            return Ok(());
        }
        for part in answer.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let path = config::expand_tilde(part);
            if is_too_broad(&path)
                && !Confirm::with_theme(&ui::theme())
                    .with_prompt(format!("{part} contains everything in it. Share it anyway?"))
                    .default(false)
                    .interact()?
            {
                continue;
            }
            if let Err(e) = cfg.add_folder(&path) {
                ui::fail(format!("{e:#}"));
            }
        }
        if !cfg.folders.is_empty()
            && !Confirm::with_theme(&ui::theme()).with_prompt("Add another path?").default(false).interact()?
        {
            return Ok(());
        }
    }
}

fn is_too_broad(path: &std::path::Path) -> bool {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical.parent().is_none() || dirs::home_dir().is_some_and(|h| canonical == h)
}

// ---- Step 2: edit permissions -----------------------------------------------------------

fn pick_writable(cfg: &mut Config) -> Result<()> {
    ui::info(dim("Leave everything unchecked to stay read-only. Every edit is backed up and can be undone."));
    ui::blank();
    let items: Vec<String> = cfg.folders.iter().map(|f| format!("{}  {}", f.label, dim(ui::tilde(&f.path)))).collect();
    let defaults: Vec<bool> = cfg.folders.iter().map(|f| f.writable()).collect();
    let picked = MultiSelect::with_theme(&ui::theme())
        .with_prompt(format!("Let your AI edit files in {}", dim("· space to select, enter to confirm")))
        .items(&items)
        .defaults(&defaults)
        .report(false)
        .interact_opt()?
        .unwrap_or_default();
    for (i, f) in cfg.folders.iter_mut().enumerate() {
        f.access = if picked.contains(&i) { "write".into() } else { "read".into() };
    }
    let writable: Vec<&str> = cfg.folders.iter().filter(|f| f.writable()).map(|f| f.label.as_str()).collect();
    if writable.is_empty() {
        ui::ok("Everything read-only");
    } else {
        ui::ok(format!("Edits allowed in {}", bold(writable.join(", "))));
    }
    Ok(())
}

// ---- Step 3: connect --------------------------------------------------------------------

fn install_service_quietly() -> Result<()> {
    if !service::supported() {
        ui::warn("No background service on this OS yet. Keep `aiva run` open in a terminal.");
        return Ok(());
    }
    let pb = ui::spinner("Starting the background service…");
    match service::install(&service::current_exe()?) {
        Ok(()) => ui::done(pb, format!("Running in the background {}", dim("· starts at login"))),
        Err(e) => {
            pb.finish_and_clear();
            return Err(e);
        }
    }
    if service::linger_enabled() == Some(false) && !service::enable_linger() {
        ui::warn("To keep it running after you log out: sudo loginctl enable-linger $USER");
    }
    Ok(())
}

pub fn connect_card(cfg: &Config) {
    let key = cfg.key.as_deref().unwrap_or("(none — run aiva setup)");
    ui::card(
        "Add aiva to your AI app",
        &[
            format!("{}    {}", dim("URL"), cyan(cfg.connector_url())),
            format!("{}  {}", dim("Token"), bold(key)),
            String::new(),
            dim("Claude: Settings → Connectors → Add custom connector"),
            dim("ChatGPT, Cursor and Claude Code work the same way."),
        ],
    );
}

/// Walks the person through adding the connector, then waits until an app actually connects.
fn guide(cfg: &Config) -> Result<()> {
    ui::blank();
    if Confirm::with_theme(&ui::theme())
        .with_prompt("Open Claude's connector settings in your browser?")
        .default(true)
        .interact()?
    {
        if ui::open_url(CLAUDE_CONNECTORS) {
            ui::ok("Opened. Click “Add custom connector”.");
        } else {
            ui::info(format!("Open {}", cyan(CLAUDE_CONNECTORS)));
        }
    }
    ui::blank();
    ui::info(format!("{} to copy the {} {}", bold("Enter"), bold("URL"), dim("· esc to skip")));
    if ui::wait_enter() {
        if ui::copy(&cfg.connector_url()) {
            ui::ok(format!("URL copied. Paste it as the connector URL, then click {}.", bold("Connect")));
        } else {
            ui::info(cyan(cfg.connector_url()));
        }
    }
    ui::info(format!("{} to copy the {} {}", bold("Enter"), bold("token"), dim("· esc to skip")));
    if ui::wait_enter() {
        if ui::copy(cfg.key.as_deref().unwrap_or_default()) {
            ui::ok("Token copied. Paste it on the aiva page that opens, then click Connect.");
        } else {
            ui::info(bold(cfg.key.as_deref().unwrap_or_default()));
        }
    }
    ui::blank();

    let pb = ui::spinner(format!("Waiting for your app to connect… {}", dim("(esc to finish later)")));
    if wait_for_app(Duration::from_secs(15 * 60)) {
        ui::done(pb, green(bold("Your AI app is connected.")));
        ui::blank();
        let example = cfg.folders.first().map(|f| f.label.as_str()).unwrap_or("my project");
        ui::info(format!("Try asking: {}", bold(format!("“Use aiva to give me an overview of {example}.”"))));
    } else {
        ui::done_warn(pb, "No app connected yet. Finish in your browser any time; `aiva status` shows the state.");
    }
    ui::blank();
    ui::info(dim("aiva status · aiva folders · aiva token · aiva doctor"));
    ui::blank();
    Ok(())
}

/// True once the background agent logs that an app asked for its tools; false on timeout or
/// when the person presses Esc / Enter / q to finish later.
fn wait_for_app(limit: Duration) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let skipped = Arc::new(AtomicBool::new(false));
    {
        let skipped = skipped.clone();
        std::thread::spawn(move || {
            let term = console::Term::stderr();
            while let Ok(key) = term.read_key() {
                if matches!(key, console::Key::Escape | console::Key::Enter | console::Key::Char('q')) {
                    skipped.store(true, Ordering::Relaxed);
                    return;
                }
            }
        });
    }
    let path = config::log_path();
    let mut pos = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let started = Instant::now();
    while started.elapsed() < limit && !skipped.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(400));
        let Ok(mut f) = std::fs::File::open(&path) else { continue };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < pos {
            pos = 0;
        }
        if len > pos && f.seek(SeekFrom::Start(pos)).is_ok() {
            let mut chunk = String::new();
            let _ = f.read_to_string(&mut chunk);
            pos = len;
            if chunk.contains(agent::APP_CONNECTED) {
                return true;
            }
        }
    }
    false
}

/// `aiva token`: show the URL and token again, with one-key copy.
pub fn token() -> Result<()> {
    let cfg = Config::require()?;
    ui::blank();
    connect_card(&cfg);
    if std::io::stdin().is_terminal() {
        ui::blank();
        ui::info(format!("{} to copy the {} {}", bold("Enter"), bold("token"), dim("· esc to skip")));
        if ui::wait_enter() && ui::copy(cfg.key.as_deref().unwrap_or_default()) {
            ui::ok("Token copied.");
        }
    }
    ui::blank();
    Ok(())
}

/// `aiva rotate`: new key (and URL). Every app connected with the old one stops working.
pub fn rotate(yes: bool) -> Result<()> {
    let mut cfg = Config::require()?;
    if !yes
        && !Confirm::with_theme(&ui::theme())
            .with_prompt("Make a new token and URL? Every connected app will be disconnected.")
            .default(false)
            .interact()?
    {
        return Ok(());
    }
    cfg.key = Some(config::generate_key()?);
    cfg.save()?;
    if service::restart_if_running()? {
        ui::ok("Restarted the background service with the new key");
    }
    ui::ok("New token created. Remove the old connector from your apps and add this one:");
    ui::blank();
    connect_card(&cfg);
    ui::blank();
    Ok(())
}

pub fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(format!("aiva/{}", crate::VERSION))
        .build()
        .into()
}

pub fn get_json(url: &str) -> Result<serde_json::Value> {
    let mut resp = http().get(url).call()?;
    Ok(resp.body_mut().read_json()?)
}
