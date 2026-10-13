//! `aiva setup` (guided onboarding), `aiva token` and `aiva rotate`.
//!
//! This computer generates its own key; the connector URL is derived from it. Connecting an app
//! means adding the URL as a custom connector and pasting the key (the "token") on the page that
//! opens. The server never stores either.

use crate::config::{self, Config};
use crate::ui::{self, bold, cyan, dim};
use crate::{agent, service};
use anyhow::{Result, bail};
use clap::Args;
use dialoguer::{Confirm, Input, theme::ColorfulTheme};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args)]
pub struct SetupArgs {
    /// Server to use
    #[arg(long, env = "AIVA_SERVER")]
    server: Option<String>,
    /// A folder Claude may read; repeat for more
    #[arg(long = "folder", value_name = "PATH")]
    folders: Vec<PathBuf>,
    /// Don't install the background service
    #[arg(long)]
    no_service: bool,
}

pub fn run(args: SetupArgs) -> Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let theme = ColorfulTheme::default();
    let mut cfg = Config::load_or_default()?;
    if let Some(server) = args.server {
        cfg.server = server.trim_end_matches('/').to_string();
    }

    ui::heading("aiva setup");
    ui::info(dim("Your AI apps will be able to read (never change, unless you allow it) the folders you pick."));

    // 1. Folders
    ui::heading("1. Choose folders to share");
    for path in &args.folders {
        let folder = cfg.add_folder(path)?;
        ui::ok(format!("{}  {}", bold(&folder.label), dim(ui::tilde(&folder.path))));
    }
    if args.folders.is_empty() {
        if !cfg.folders.is_empty() {
            for f in &cfg.folders {
                ui::ok(format!("{}  {}", bold(&f.label), dim(ui::tilde(&f.path))));
            }
            if interactive && Confirm::with_theme(&theme).with_prompt("Add more folders?").default(false).interact()? {
                prompt_folders(&theme, &mut cfg)?;
            }
        } else if interactive {
            prompt_folders(&theme, &mut cfg)?;
        } else {
            bail!("--folder is required when not running in a terminal");
        }
    }
    ui::info(dim("Change these any time with `aiva folders add|remove`; allow edits with `aiva folders set <name> write`."));

    // 2. Key (kept if it already exists, so the URL stays the same)
    if cfg.key.is_none() {
        cfg.key = Some(config::generate_key()?);
    }
    cfg.save()?;

    // 3. Background service + check the server sees us
    if !args.no_service {
        ui::heading("2. Run in the background");
        install_service()?;
    }
    match crate::tokio_main(agent::verify(&cfg)) {
        Ok(_) => ui::ok(format!("Connected to {}", cfg.server)),
        Err(e) => ui::warn(format!("Couldn't reach the server yet ({e}). aiva will keep retrying in the background.")),
    }

    ui::heading("3. Connect your AI app");
    print_connect(&cfg);
    Ok(())
}

/// The URL + token block shown after setup and by `aiva token`.
pub fn print_connect(cfg: &Config) {
    ui::info("In Claude: Settings → Connectors → Add custom connector (ChatGPT, Cursor and other MCP apps work the same way).");
    ui::info(format!("  URL    {}", cyan(cfg.connector_url())));
    ui::info(format!("  Token  {}", bold(cfg.key.as_deref().unwrap_or("(none — run aiva setup)"))));
    ui::info("Add the URL, click Connect, and paste the token on the page that opens.");
    ui::info(dim("Keep the token private: anyone with it can use your shared folders. `aiva rotate` replaces it."));
}

/// `aiva token`: show the URL and token again.
pub fn token() -> Result<()> {
    let cfg = Config::require()?;
    print_connect(&cfg);
    Ok(())
}

/// `aiva rotate`: new key (and URL). Every app connected with the old one stops working.
pub fn rotate(yes: bool) -> Result<()> {
    let mut cfg = Config::require()?;
    if !yes {
        let sure = Confirm::new()
            .with_prompt("Make a new token and URL? Every app connected now will be disconnected and must be added again.")
            .default(false)
            .interact()?;
        if !sure {
            return Ok(());
        }
    }
    cfg.key = Some(config::generate_key()?);
    cfg.save()?;
    if service::restart_if_running()? {
        ui::ok("Restarted the background service with the new key");
    }
    ui::ok("New token created. Remove the old connector from your apps and add this one:");
    print_connect(&cfg);
    Ok(())
}

fn install_service() -> Result<()> {
    if !service::supported() {
        ui::warn("A background service isn't available on this OS yet. Keep `aiva run` open in a terminal.");
        return Ok(());
    }
    service::install(&service::current_exe()?)?;
    ui::ok(format!("Running in the background and starting at login ({})", dim(service::describe())));
    if service::linger_enabled() == Some(false) {
        if service::enable_linger() {
            ui::ok("Enabled systemd linger so it keeps running after you log out");
        } else {
            ui::warn("To keep it running after you log out, run: sudo loginctl enable-linger $USER");
        }
    }
    Ok(())
}

fn prompt_folders(theme: &ColorfulTheme, cfg: &mut Config) -> Result<()> {
    let cwd = std::env::current_dir().ok().filter(|d| Some(d) != dirs::home_dir().as_ref());
    loop {
        let mut input = Input::<String>::with_theme(theme).with_prompt("Folder path (comma-separate several)");
        if let Some(cwd) = &cwd {
            if cfg.folders.is_empty() {
                input = input.default(ui::tilde(cwd));
            }
        }
        let answer: String = input.interact_text()?;
        for part in answer.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let path = config::expand_tilde(part);
            if is_too_broad(&path) {
                let sure = Confirm::with_theme(theme)
                    .with_prompt(format!("{part} contains everything in it (documents, downloads...). Share it anyway?"))
                    .default(false)
                    .interact()?;
                if !sure {
                    continue;
                }
            }
            match cfg.add_folder(&path) {
                Ok(folder) => ui::ok(format!("{}  {}", bold(&folder.label), dim(ui::tilde(&folder.path)))),
                Err(e) => ui::fail(format!("{e:#}")),
            }
        }
        if !cfg.folders.is_empty() && !Confirm::with_theme(theme).with_prompt("Add another folder?").default(false).interact()? {
            return Ok(());
        }
    }
}

fn is_too_broad(path: &std::path::Path) -> bool {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical.parent().is_none() || dirs::home_dir().is_some_and(|h| canonical == h)
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
