//! The terminal design system: one accent color (the orange dot from the website), quiet grays,
//! consistent indentation, step headers, cards, spinners and a matching prompt theme.

use console::{Style, StyledObject, Term, measure_text_width, style};
use dialoguer::theme::ColorfulTheme;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

/// 208 = the signal orange used on the web pages.
const ACCENT: u8 = 208;

pub fn accent(msg: impl std::fmt::Display) -> String {
    style(msg).color256(ACCENT).to_string()
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

pub fn green(msg: impl std::fmt::Display) -> String {
    style(msg).green().to_string()
}

pub fn ok(msg: impl std::fmt::Display) {
    println!("  {} {msg}", style("✓").green());
}

pub fn warn(msg: impl std::fmt::Display) {
    println!("  {} {msg}", style("!").yellow().bold());
}

pub fn fail(msg: impl std::fmt::Display) {
    eprintln!("  {} {msg}", style("✗").red().bold());
}

pub fn info(msg: impl std::fmt::Display) {
    println!("  {msg}");
}

pub fn heading(msg: impl std::fmt::Display) {
    println!("\n  {}", style(msg).bold());
}

pub fn blank() {
    println!();
}

fn width() -> usize {
    Term::stdout().size_checked().map(|(_, w)| w as usize).unwrap_or(80).clamp(40, 100)
}

/// The opening lines of `aiva setup`.
pub fn banner(subtitle: &str) {
    println!();
    println!("  {}  {}  {}", style("●").color256(ACCENT), style("aiva").bold(), dim(format!("v{}", crate::VERSION)));
    println!("     {}", dim(subtitle));
    println!();
}

/// `── 1 of 3 · Choose folders ──────────`
pub fn step(n: usize, total: usize, title: &str) {
    let label = format!(" {n} of {total} · {title} ");
    let line = width().min(72) - 4;
    let rule = "─".repeat(line.saturating_sub(2 + measure_text_width(&label)));
    println!();
    println!("  {}{}{}", dim("──"), style(label).bold(), dim(rule));
    println!();
}

/// A rounded box around `lines` (already styled). Falls back to plain lines on narrow terminals.
pub fn card(title: &str, lines: &[String]) {
    let inner = lines.iter().map(|l| measure_text_width(l)).chain([measure_text_width(title)]).max().unwrap_or(0) + 4;
    if inner + 4 > width() {
        println!("  {}", bold(title));
        for l in lines {
            println!("  {l}");
        }
        return;
    }
    let pad = |s: &str| format!("{s}{}", " ".repeat(inner.saturating_sub(measure_text_width(s) + 2)));
    println!("  {}", dim(format!("╭{}╮", "─".repeat(inner))));
    println!("  {} {} {}", dim("│"), pad(&bold(title)), dim("│"));
    println!("  {} {} {}", dim("│"), pad(""), dim("│"));
    for l in lines {
        println!("  {} {} {}", dim("│"), pad(l), dim("│"));
    }
    println!("  {}", dim(format!("╰{}╯", "─".repeat(inner))));
}

/// A spinner line; finish it with `done` / `done_warn`.
pub fn spinner(msg: impl Into<String>) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("  {spinner:.208} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"]),
    );
    pb.set_message(msg.into());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

pub fn done(pb: ProgressBar, msg: impl std::fmt::Display) {
    pb.finish_and_clear();
    ok(msg);
}

pub fn done_warn(pb: ProgressBar, msg: impl std::fmt::Display) {
    pb.finish_and_clear();
    warn(msg);
}

/// Prompt theme matching the rest: orange pointer and checks, gray hints.
pub fn theme() -> ColorfulTheme {
    let s = |t: &str, st: Style| -> StyledObject<String> { st.apply_to(t.to_string()) };
    let accent = Style::new().color256(ACCENT);
    ColorfulTheme {
        defaults_style: Style::new().dim(),
        prompt_style: Style::new().bold(),
        prompt_prefix: s(" ", Style::new()),
        prompt_suffix: s("›", Style::new().dim()),
        success_prefix: s(" ✓", Style::new().green()),
        success_suffix: s("·", Style::new().dim()),
        error_prefix: s(" ✗", Style::new().red()),
        error_style: Style::new().red(),
        hint_style: Style::new().dim(),
        values_style: accent.clone(),
        active_item_style: Style::new().bold(),
        inactive_item_style: Style::new(),
        active_item_prefix: s("  ›", accent.clone()),
        inactive_item_prefix: s("   ", Style::new()),
        checked_item_prefix: s("  ◉", accent.clone()),
        unchecked_item_prefix: s("  ○", Style::new().dim()),
        picked_item_prefix: s("  ›", accent),
        unpicked_item_prefix: s("   ", Style::new()),
    }
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

/// Copies text to the system clipboard. Returns false when no clipboard tool is available.
pub fn copy(text: &str) -> bool {
    use std::io::Write;
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    };
    for (cmd, args) in tools {
        if let Ok(mut child) = std::process::Command::new(cmd).args(*args).stdin(std::process::Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return true;
            }
        }
    }
    false
}

/// Opens a URL in the default browser.
pub fn open_url(url: &str) -> bool {
    let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(cmd).arg(url).status().is_ok_and(|s| s.success())
}

/// Waits for Enter. Returns false if the user pressed Esc or q.
pub fn wait_enter() -> bool {
    let term = Term::stderr();
    loop {
        match term.read_key() {
            Ok(console::Key::Enter) => return true,
            Ok(console::Key::Escape) | Ok(console::Key::Char('q')) => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}
