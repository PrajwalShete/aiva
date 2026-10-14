//! `aiva setup` (guided onboarding), `aiva token` and `aiva rotate`.
//!
//! This computer generates its own key; the connector URL is derived from it. Connecting an app
//! means adding the URL as a custom connector and pasting the key (the "token") on the page that
//! opens. The server never stores either.

use crate::config::{self, Config};
use crate::ui::{self, bold, cyan, dim, green};
use crate::screen::Screen;
use crate::{agent, discover, picker, service};
use anyhow::{Result, bail};
use clap::Args;
use dialoguer::Confirm;
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

    // Picking folders runs full screen with the mark animating throughout; the rest stays plain.
    let picking = interactive && args.folders.is_empty();
    let mut screen = Screen::start(picking);
    let search = if picking { Some(intro(&mut screen)) } else { None };
    screen.banner();

    // ── 1. Folders ──
    screen.step(1, total, "Choose folders");
    if cfg.key.is_some() && !cfg.folders.is_empty() {
        screen.info(dim("Already set up. Running setup again keeps your URL and token; change anything below."));
    }
    if !args.folders.is_empty() {
        for path in &args.folders {
            let folder = cfg.add_folder(path)?;
            screen.ok(format!("{}  {}", bold(&folder.label), dim(ui::tilde(&folder.path))));
        }
    } else if let Some((found, index)) = search {
        pick_folders(&mut screen, &mut cfg, found, index)?;
    } else if cfg.folders.is_empty() {
        bail!("--folder is required when not running in a terminal");
    }

    // ── 2. Edits ──
    screen.step(2, total, "Allow edits (optional)");
    if picking {
        pick_writable(&mut screen, &mut cfg)?;
    } else {
        screen.info(dim("Read-only. Allow edits later with `aiva folders set <name> write`."));
    }
    if cfg.key.is_none() {
        cfg.key = Some(config::generate_key()?);
    }
    cfg.save()?;

    // ── 3. Connect ──
    screen.step(3, total, "Connect your AI app");
    if !args.no_service {
        install_service_quietly(&mut screen)?;
    }
    let probe = cfg.clone();
    match screen.work("Connecting to the aiva relay…", move || crate::tokio_main(agent::verify(&probe))) {
        Ok(_) => screen.ok(format!("Connected to {}", dim(&cfg.server))),
        Err(e) => screen.warn(format!("Couldn't reach the relay yet ({e}). aiva keeps retrying in the background.")),
    }
    screen.blank();
    screen.card("Add aiva to your AI app", &connect_lines(&cfg));

    if interactive {
        guide(&mut screen, &cfg)?;
    } else {
        screen.info(dim("Add the URL as a custom connector, click Connect, and paste the token."));
    }
    screen.finish();
    Ok(())
}

/// `AIVA_TRACE=1 aiva setup` prints how long each step takes, to diagnose slow machines.
fn trace(msg: &str) {
    if std::env::var_os("AIVA_TRACE").is_some() {
        eprintln!("  {}", dim(format!("[trace] {msg}")));
    }
}

// ---- Step 1: folder picker --------------------------------------------------------------

/// The opening: the mark animates while this computer is searched for projects (and indexed
/// for the picker's search), for at least one full wave so it never just flickers.
fn intro(screen: &mut Screen) -> (Vec<discover::Project>, std::sync::Arc<discover::FolderIndex>) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let index = discover::index_folders();
    let started = Instant::now();
    let count = Arc::new(AtomicUsize::new(0));
    let search = {
        let count = count.clone();
        std::thread::spawn(move || discover::projects_within(30, Duration::from_secs(4), |n, _| count.store(n, Ordering::Relaxed)))
    };
    let nothing = |_: usize| Vec::new();
    while !search.is_finished() {
        let n = count.load(Ordering::Relaxed);
        screen.set_status(vec![format!("{} Looking for your projects… {}", ui::spin(), dim(format!("{n} found")))]);
        screen.poll(0, &nothing, Some(Duration::from_millis(66)));
    }
    let found = search.join().unwrap_or_default();
    trace(&format!("project search: {} found in {} ms", found.len(), started.elapsed().as_millis()));
    let summary = match found.len() {
        0 => dim("Ready").to_string(),
        1 => format!("{} {}", green("✓"), "Found 1 project"),
        n => format!("{} Found {n} projects", green("✓")),
    };
    screen.set_status(vec![summary]);
    if let Some(rest) = Duration::from_millis(1800).checked_sub(started.elapsed()) {
        screen.poll(0, &nothing, Some(rest));
    }
    (found, index)
}

fn pick_folders(screen: &mut Screen, cfg: &mut Config, found: Vec<discover::Project>, index: std::sync::Arc<discover::FolderIndex>) -> Result<()> {
    screen.info(dim("Your AI can read only these folders, and never secret files like .env or keys."));
    screen.blank();
    // Already-shared folders first (checked), then discovered projects, newest first.
    let mut rows: Vec<picker::Row> = cfg
        .folders
        .iter()
        .map(|f| picker::Row { path: f.path.clone(), name: f.label.clone(), kind: "shared".into(), when: String::new() })
        .collect();
    for p in found {
        if !rows.iter().any(|r| r.path == p.path) {
            rows.push(picker::Row { path: p.path, name: p.name, kind: p.kind, when: crate::tools::ago(p.modified) });
        }
    }
    let shared: Vec<PathBuf> = cfg.folders.iter().map(|f| f.path.clone()).collect();
    ui::flush_input();
    let mut chosen = picker::folders(screen, "Folders to share", rows, &shared, index)?;
    // Sharing a whole home folder or disk is almost never what someone means.
    let mut kept = Vec::new();
    for path in chosen.drain(..) {
        if is_too_broad(&path) && !screen.confirm(&format!("{} contains everything in it. Share it anyway?", ui::tilde(&path)), false)? {
            continue;
        }
        kept.push(path);
    }
    cfg.folders.retain(|f| kept.contains(&f.path));
    for path in &kept {
        if let Err(e) = cfg.add_folder(path) {
            screen.fail(format!("{e:#}"));
        }
    }
    if cfg.folders.is_empty() {
        screen.warn("No folders shared yet. Pick at least one.");
        screen.blank();
        return pick_folders(screen, cfg, Vec::new(), discover::index_folders());
    }
    for f in &cfg.folders {
        screen.ok(format!("{}  {}", bold(&f.label), dim(ui::tilde(&f.path))));
    }
    Ok(())
}

fn is_too_broad(path: &std::path::Path) -> bool {
    let canonical = crate::platform::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical.parent().is_none() || dirs::home_dir().is_some_and(|h| canonical == h)
}

// ---- Step 2: edit permissions -----------------------------------------------------------

fn pick_writable(screen: &mut Screen, cfg: &mut Config) -> Result<()> {
    screen.info(dim("Leave everything unchecked to stay read-only. Every edit is backed up and can be undone."));
    screen.blank();
    let items: Vec<String> = cfg.folders.iter().map(|f| format!("{}  {}", f.label, dim(ui::tilde(&f.path)))).collect();
    let defaults: Vec<bool> = cfg.folders.iter().map(|f| f.writable()).collect();
    ui::flush_input();
    let picked = screen.multi_select("Let your AI edit files in", &items, &defaults)?;
    for (i, f) in cfg.folders.iter_mut().enumerate() {
        f.access = if picked.contains(&i) { "write".into() } else { "read".into() };
    }
    let writable: Vec<&str> = cfg.folders.iter().filter(|f| f.writable()).map(|f| f.label.as_str()).collect();
    if writable.is_empty() {
        screen.ok("Everything read-only");
    } else {
        screen.ok(format!("Edits allowed in {}", bold(writable.join(", "))));
    }
    Ok(())
}

// ---- Step 3: connect --------------------------------------------------------------------

fn install_service_quietly(screen: &mut Screen) -> Result<()> {
    if !service::supported() {
        screen.warn("No background service on this OS yet. Keep `aiva run` open in a terminal.");
        return Ok(());
    }
    let exe = service::current_exe()?;
    screen.work("Starting the background service…", move || service::install(&exe))?;
    screen.ok(format!("Running in the background {}", dim("· starts at login")));
    if service::linger_enabled() == Some(false) && !service::enable_linger() {
        screen.warn("To keep it running after you log out: sudo loginctl enable-linger $USER");
    }
    Ok(())
}

fn connect_lines(cfg: &Config) -> Vec<String> {
    let key = cfg.key.as_deref().unwrap_or("(none — run aiva setup)");
    vec![
        format!("{}    {}", dim("URL"), cyan(cfg.connector_url())),
        format!("{}  {}", dim("Token"), bold(key)),
        String::new(),
        dim("Claude: Settings → Connectors → Add custom connector"),
        dim("ChatGPT, Cursor and Claude Code work the same way."),
    ]
}

pub fn connect_card(cfg: &Config) {
    ui::card("Add aiva to your AI app", &connect_lines(cfg));
}

/// Walks the person through adding the connector, then waits until an app actually connects.
fn guide(screen: &mut Screen, cfg: &Config) -> Result<()> {
    screen.blank();
    ui::flush_input();
    if screen.confirm("Open Claude's connector settings in your browser?", true)? {
        if ui::open_url(CLAUDE_CONNECTORS) {
            screen.ok("Opened. Click “Add custom connector”.");
        } else {
            screen.info(format!("Open {}", cyan(CLAUDE_CONNECTORS)));
        }
    }
    screen.blank();
    if screen.press_enter(&format!("{} to copy the {} {}", bold("Enter"), bold("URL"), dim("· esc to skip"))) {
        if ui::copy(&cfg.connector_url()) {
            screen.ok(format!("URL copied. Paste it as the connector URL, then click {}.", bold("Connect")));
        } else {
            screen.info(cyan(cfg.connector_url()));
        }
    }
    if screen.press_enter(&format!("{} to copy the {} {}", bold("Enter"), bold("token"), dim("· esc to skip"))) {
        if ui::copy(cfg.key.as_deref().unwrap_or_default()) {
            screen.ok("Token copied. Paste it on the aiva page that opens, then click Connect.");
        } else {
            screen.info(bold(cfg.key.as_deref().unwrap_or_default()));
        }
    }
    screen.blank();

    let mut log = LogWatch::new();
    let waiting = format!("Waiting for your app to connect… {}", dim("(esc to finish later)"));
    if screen.wait_until(&waiting, Duration::from_secs(15 * 60), || log.app_connected()) {
        screen.ok(green(bold("Your AI app is connected.")));
        screen.blank();
        let example = cfg.folders.first().map(|f| f.label.as_str()).unwrap_or("my project");
        screen.info(format!("Try asking: {}", bold(format!("“Use aiva to give me an overview of {example}.”"))));
    } else {
        screen.warn("No app connected yet. Finish in your browser any time; `aiva status` shows the state.");
    }
    screen.blank();
    screen.info(dim("aiva status · aiva folders · aiva token · aiva doctor"));
    screen.blank();
    Ok(())
}

/// Follows the background agent's log for the line it writes when an app asks for its tools.
struct LogWatch {
    path: PathBuf,
    pos: u64,
}

impl LogWatch {
    fn new() -> Self {
        let path = config::log_path();
        let pos = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self { path, pos }
    }

    fn app_connected(&mut self) -> bool {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(&self.path) else { return false };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.pos {
            self.pos = 0;
        }
        if len > self.pos && f.seek(SeekFrom::Start(self.pos)).is_ok() {
            let mut chunk = String::new();
            let _ = f.read_to_string(&mut chunk);
            self.pos = len;
            return chunk.contains(agent::APP_CONNECTED);
        }
        false
    }
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
