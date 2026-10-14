mod agent;
mod commands;
mod config;
mod discover;
mod picker;
mod platform;
mod protocol;
mod screen;
mod service;
mod setup;
mod tools;
mod ui;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "aiva",
    version,
    about = "Let Claude read and explain the code on this computer, from any Claude app.",
    after_help = "Get started:  aiva setup\nDocs:         https://aiva.prajwalshete.com"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Guided setup: choose folders, start the background service, show the URL and token
    Setup(setup::SetupArgs),
    /// Show the connector URL and the token to paste when connecting an app
    Token,
    /// Replace the token (and URL); every connected app is disconnected
    Rotate {
        #[arg(short, long)]
        yes: bool,
    },
    /// Show the connector URL, connection state, service and shared folders
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List, add or remove the folders Claude can read
    #[command(subcommand_required = false)]
    Folders {
        #[command(subcommand)]
        action: Option<FolderAction>,
    },
    /// Manage the background service (launchd on macOS, systemd on Linux)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Start the background service
    Start,
    /// Stop the background service (Claude can't reach this computer while stopped)
    Stop,
    /// Restart the background service
    Restart,
    /// Run the agent in the foreground (this is what the service runs)
    Run,
    /// Show the agent log
    Logs {
        /// Number of lines to show
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Keep printing new lines
        #[arg(short, long)]
        follow: bool,
    },
    /// Check the setup and explain how to fix problems
    Doctor,
    /// Print the URL to paste into Claude as a custom connector
    Url,
    /// List the tools Claude can use
    Tools,
    /// Run a tool locally, exactly as Claude would (for testing)
    Call {
        tool: String,
        /// Arguments as JSON, e.g. '{"path": "Jev/README.md"}'
        #[arg(default_value = "{}")]
        args: String,
    },
    /// Show recent file changes made by Claude
    History {
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// Revert the latest change made by Claude (or a specific one from `aiva history`)
    Undo { id: Option<u64> },
    /// Show the config file location and contents (the token is hidden)
    Config,
    /// Download and install the latest version
    Update,
    /// Stop and remove the service, the config and (unless --keep-binary) this program
    Uninstall {
        #[arg(long)]
        keep_binary: bool,
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum FolderAction {
    /// List shared folders
    List,
    /// Share one or more folders with Claude (read-only unless --write)
    Add {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Also let Claude create and edit files in these folders
        #[arg(long)]
        write: bool,
    },
    /// Change a folder's access: "read" (default) or "write" (Claude may create and edit files)
    Set {
        /// Folder name or path
        target: String,
        #[arg(value_parser = ["read", "write"])]
        access: String,
    },
    /// Stop sharing folders (by name or path)
    Remove {
        #[arg(required = true)]
        targets: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the service and start it at login
    Install,
    /// Remove the service
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
}

fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    platform::init_terminal();
    platform::remove_replaced_exe();
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        let result = home();
        if let Err(e) = result {
            ui::fail(format!("{e:#}"));
            std::process::exit(1);
        }
        return;
    };
    let result = match command {
        Command::Setup(args) => setup::run(args),
        Command::Token => setup::token(),
        Command::Rotate { yes } => setup::rotate(yes),
        Command::Status { json } => commands::status(json),
        Command::Folders { action } => match action {
            None | Some(FolderAction::List) => commands::folders_list(),
            Some(FolderAction::Add { paths, write }) => commands::folders_add(&paths, write),
            Some(FolderAction::Set { target, access }) => commands::folders_set(&target, &access),
            Some(FolderAction::Remove { targets }) => commands::folders_remove(&targets),
        },
        Command::Service { action } => match action {
            ServiceAction::Install => commands::service_install(),
            ServiceAction::Uninstall => commands::service_uninstall(),
            ServiceAction::Start => commands::start(),
            ServiceAction::Stop => commands::stop(),
            ServiceAction::Restart => commands::restart(),
            ServiceAction::Status => commands::service_status(),
        },
        Command::Start => commands::start(),
        Command::Stop => commands::stop(),
        Command::Restart => commands::restart(),
        Command::Run => tokio_main(agent::run()),
        Command::Logs { lines, follow } => commands::logs(lines, follow),
        Command::Doctor => commands::doctor(),
        Command::Url => commands::url(),
        Command::Tools => commands::tools(),
        Command::Call { tool, args } => commands::call(&tool, &args),
        Command::History { lines } => commands::history(lines),
        Command::Undo { id } => commands::undo(id),
        Command::Config => commands::show_config(),
        Command::Update => commands::update(),
        Command::Uninstall { keep_binary, yes } => commands::uninstall(keep_binary, yes),
    };
    if let Err(e) = result {
        ui::fail(format!("{e:#}"));
        std::process::exit(1);
    }
}

/// `aiva` with no command: the welcome screen with a "Get started" menu.
fn home() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        <Cli as clap::CommandFactory>::command().print_help()?;
        return Ok(());
    }
    let set_up = config::Config::load().ok().flatten().is_some_and(|c| c.key.is_some());
    let items: [(&str, &str); 6] = [
        ("aiva setup", "share folders and connect your AI"),
        ("aiva status", "is everything connected?"),
        ("aiva token", "URL and token for your AI app"),
        ("aiva folders", "what your AI can see"),
        ("aiva doctor", "find and fix problems"),
        ("view all commands", ""),
    ];
    let Some(choice) = ui::hero_menu("Get started", &items, if set_up { 1 } else { 0 }) else { return Ok(()) };
    match choice {
        0 => setup::run(setup::SetupArgs::default()),
        1 => commands::status(false),
        2 => setup::token(),
        3 => commands::folders_list(),
        4 => commands::doctor(),
        _ => {
            <Cli as clap::CommandFactory>::command().print_help()?;
            Ok(())
        }
    }
}

pub fn tokio_main<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("starting async runtime").block_on(fut)
}
