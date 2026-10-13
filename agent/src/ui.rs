//! Small helpers for consistent terminal output.

use console::style;

pub fn ok(msg: impl std::fmt::Display) {
    println!("  {} {msg}", style("✓").green());
}

pub fn warn(msg: impl std::fmt::Display) {
    println!("  {} {msg}", style("!").yellow());
}

pub fn fail(msg: impl std::fmt::Display) {
    eprintln!("  {} {msg}", style("✗").red());
}

pub fn info(msg: impl std::fmt::Display) {
    println!("  {msg}");
}

pub fn heading(msg: impl std::fmt::Display) {
    println!("\n{}", style(msg).bold());
}

pub fn dim(msg: impl std::fmt::Display) -> String {
    style(msg).dim().to_string()
}

pub fn bold(msg: impl std::fmt::Display) -> String {
    style(msg).bold().to_string()
}

pub fn cyan(msg: impl std::fmt::Display) -> String {
    style(msg).cyan().to_string()
}

/// Replaces the home directory prefix with `~` for display.
pub fn tilde(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
        }
    }
    path.display().to_string()
}
