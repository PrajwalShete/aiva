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

/// The opening of `aiva setup`: the mark with the brand column beside it.
pub fn banner(subtitle: &str) {
    hero(&brand_lines(subtitle));
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

/// Discards keys typed before a prompt appeared (e.g. Enter pressed while setup was busy),
/// so they can't silently confirm the next prompt.
pub fn flush_input() {
    #[cfg(unix)]
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
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

// ---- The mark and the home screen ---------------------------------------------------------

const MARK_W: usize = 44;
const MARK_H: usize = 17;

/// The aiva mark drawn in text: a shaded orange sphere (the brand dot) with three voice waves,
/// fading from light to dark gray. Computed, so it stays crisp and consistent.
pub fn mark() -> Vec<String> {
    const RAMP: &[u8] = b" .:-=+*#%";
    let (cx, cy, aspect) = (10.5f64, 8.5f64 * 2.05, 2.05f64);
    let dot_r = 8.4;
    let waves = [(13.6, 1.9, 252u8), (19.4, 1.9, 246u8), (25.2, 1.9, 240u8)];
    let limit = 50f64.to_radians();
    let mut lines = Vec::with_capacity(MARK_H);
    for y in 0..MARK_H {
        let mut line = String::new();
        for x in 0..MARK_W {
            let (px, py) = (x as f64 + 0.5, (y as f64 + 0.5) * aspect);
            let (dx, dy) = (px - cx, py - cy);
            let d = dx.hypot(dy);
            // Sphere: coverage from the edge, shade from a light at the top-left.
            let cover = ((dot_r - d) / 1.6 + 0.5).clamp(0.0, 1.0);
            let mut best = (cover, None::<u8>);
            if cover > 0.05 {
                let nx = dx / dot_r;
                let ny = dy / dot_r;
                let nz = (1.0 - (nx * nx + ny * ny).min(1.0)).sqrt();
                let light = ((-0.45 * nx - 0.55 * ny + 0.70 * nz).max(0.0) * 0.85 + 0.15).clamp(0.0, 1.0);
                let color = match light {
                    l if l > 0.80 => 223,
                    l if l > 0.62 => 215,
                    l if l > 0.45 => 209,
                    l if l > 0.30 => 202,
                    _ => 166,
                };
                best = (cover * (0.55 + 0.45 * light), Some(color));
            }
            let angle = dy.atan2(dx).abs();
            if dx > 0.0 && angle < limit {
                let fall = ((limit - angle) / 14f64.to_radians()).clamp(0.0, 1.0);
                for (radius, thickness, gray) in waves {
                    let v = (1.0 - (d - radius).abs() / thickness).max(0.0) * fall;
                    if v > best.0 {
                        best = (v, Some(gray));
                    }
                }
            }
            let idx = ((best.0 * (RAMP.len() - 1) as f64) + 0.35) as usize;
            let ch = RAMP[idx.min(RAMP.len() - 1)] as char;
            match best.1 {
                Some(color) if ch != ' ' => line.push_str(&style(ch).color256(color).to_string()),
                _ => line.push(' '),
            }
        }
        lines.push(line);
    }
    lines
}

/// The mark on the left and `right` (already styled) on the right, vertically centered.
/// Stacks them on narrow terminals and drops the mark when there's no room.
pub fn hero(right: &[String]) {
    let w = Term::stdout().size_checked().map(|(_, w)| w as usize).unwrap_or(80);
    let mark = mark();
    let right_w = right.iter().map(|l| measure_text_width(l)).max().unwrap_or(0);
    println!();
    if w >= MARK_W + 6 + right_w {
        let top = (MARK_H.saturating_sub(right.len())) / 2;
        for (i, art) in mark.iter().enumerate() {
            let text = i.checked_sub(top).and_then(|j| right.get(j)).map(String::as_str).unwrap_or("");
            println!("  {art}    {text}");
        }
    } else {
        if w >= MARK_W + 4 {
            for art in &mark {
                println!("  {art}");
            }
            println!();
        }
        for l in right {
            println!("  {l}");
        }
    }
    println!();
}

/// Name, tagline, version and divider: the top of the right column.
pub fn brand_lines(subtitle: &str) -> Vec<String> {
    vec![
        style("aiva").color256(ACCENT).bold().to_string(),
        "Talk to your code from anywhere.".to_string(),
        String::new(),
        dim(format!("v{}", crate::VERSION)),
        String::new(),
        dim("─".repeat(34)),
        String::new(),
        style(subtitle).bold().to_string(),
    ]
}

/// An arrow-key menu drawn in place inside the right column of the hero. Returns the chosen index.
pub fn hero_menu(subtitle: &str, items: &[(&str, &str)], start: usize) -> Option<usize> {
    let term = Term::stdout();
    let mut current = start.min(items.len().saturating_sub(1));
    let render = |current: usize| -> Vec<String> {
        let mut right = brand_lines(subtitle);
        right.push(String::new());
        for (i, (cmd, desc)) in items.iter().enumerate() {
            let line = if i == current {
                format!("{} {}  {}", style("›").color256(ACCENT).bold(), style(cmd).color256(ACCENT).bold(), dim(format!("({desc})")))
            } else if desc.is_empty() {
                format!("  {}", dim(cmd))
            } else {
                format!("  {cmd}")
            };
            right.push(line);
        }
        right.push(String::new());
        right.push(dim("↑↓ to move · enter to run · q to quit"));
        right
    };
    let height = |right: &[String]| {
        let w = term.size_checked().map(|(_, w)| w as usize).unwrap_or(80);
        let right_w = right.iter().map(|l| measure_text_width(l)).max().unwrap_or(0);
        if w >= MARK_W + 6 + right_w { MARK_H.max(right.len()) + 2 } else if w >= MARK_W + 4 { MARK_H + 1 + right.len() + 2 } else { right.len() + 2 }
    };
    let _ = term.hide_cursor();
    let mut right = render(current);
    hero(&right);
    let chosen = loop {
        let key = term.read_key();
        match key {
            Ok(console::Key::ArrowUp) | Ok(console::Key::Char('k')) => current = current.checked_sub(1).unwrap_or(items.len() - 1),
            Ok(console::Key::ArrowDown) | Ok(console::Key::Char('j')) | Ok(console::Key::Tab) => current = (current + 1) % items.len(),
            Ok(console::Key::Enter) => break Some(current),
            Ok(console::Key::Escape) | Ok(console::Key::Char('q')) | Err(_) => break None,
            _ => continue,
        }
        let lines = height(&right);
        let _ = term.move_cursor_up(lines);
        let _ = term.clear_to_end_of_screen();
        right = render(current);
        hero(&right);
    };
    let _ = term.show_cursor();
    chosen
}
