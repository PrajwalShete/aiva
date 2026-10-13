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

const MARK_W: usize = 40;
const MARK_H: usize = 17;

/// The aiva mark drawn in text: the brand dot eclipsed into a dark disc, haloed by an orange
/// corona with a bright flare at the lower right. Computed, so it stays crisp and consistent.
pub fn mark() -> Vec<String> {
    mark_frame(0.3)
}

/// One frame of the mark at `t` seconds: rings of light ripple outward through the corona like
/// sound waves, the streamers drift slowly and the flare breathes.
pub fn mark_frame(t: f64) -> Vec<String> {
    const RAMP: &[u8] = b" .:-=+*#%@";
    let sample = |i: f64, j: f64| -> f64 {
        let x = (i - (MARK_W as f64 - 1.0) / 2.0) / 2.0;
        let y = j - (MARK_H as f64 - 1.0) / 2.0;
        let r = x.hypot(y);
        let disc = 5.3;
        if (x + 0.35).hypot(y + 0.25) < disc {
            return 0.0;
        }
        let angle = y.atan2(x);
        let d = (r - disc + 0.4).max(0.0);
        let bias = 0.5 + 0.5 * (angle - 0.75).cos().max(0.0).powf(1.5);
        let stream = 0.8 + 0.2 * (angle * 5.0 + 0.7 - 0.5 * t).cos();
        let mut v = ((-d / 1.1).exp() + 0.3 * (-d / 2.6).exp()) * (0.62 + 0.38 * bias) * stream * 1.3;
        let phase = (d / 2.4 - t / 1.8).rem_euclid(1.0);
        let wave = (-((phase - 0.5) / 0.13).powi(2)).exp();
        v += 0.45 * wave * ((8.3 - r) / 2.2).clamp(0.0, 1.0) * (0.7 + 0.3 * bias);
        let (fx, fy) = (x - 3.75, y - 3.75);
        v += (-(fx * fx * 0.8 + fy * fy) / 0.5).exp() * 1.5 * (1.0 + 0.15 * (t * 2.6).sin());
        v.min(1.0)
    };
    let mut lines = Vec::with_capacity(MARK_H);
    for j in 0..MARK_H {
        let mut line = String::new();
        for i in 0..MARK_W {
            let mut sum = 0.0;
            for a in 0..3 {
                for b in 0..3 {
                    sum += sample(i as f64 + (a as f64 + 0.5) / 3.0 - 0.5, j as f64 + (b as f64 + 0.5) / 3.0 - 0.5);
                }
            }
            let v = sum / 9.0;
            if v < 0.11 {
                line.push(' ');
                continue;
            }
            let ch = RAMP[((v * RAMP.len() as f64) as usize).min(RAMP.len() - 1)] as char;
            let color = match v * 1.05 {
                c if c >= 0.90 => 230,
                c if c >= 0.78 => 223,
                c if c >= 0.62 => 215,
                c if c >= 0.45 => 208,
                c if c >= 0.32 => 202,
                c if c >= 0.20 => 166,
                _ => 130,
            };
            line.push_str(&style(ch).color256(color).to_string());
        }
        lines.push(line);
    }
    lines
}

/// The mark on the left and `right` (already styled) on the right, vertically centered.
/// Stacks them on narrow terminals and drops the mark when there's no room.
pub fn hero(right: &[String]) {
    print!("{}", hero_text(right, &mark()));
}

/// The hero as text, every line starting with "clear line" so it can be redrawn in place.
fn hero_text(right: &[String], mark: &[String]) -> String {
    use std::fmt::Write;
    let w = Term::stdout().size_checked().map(|(_, w)| w as usize).unwrap_or(80);
    let right_w = right.iter().map(|l| measure_text_width(l)).max().unwrap_or(0);
    let mut out = String::from("\x1b[2K\n");
    if w >= MARK_W + 6 + right_w {
        let top = (MARK_H.saturating_sub(right.len())) / 2;
        for (i, art) in mark.iter().enumerate() {
            let text = i.checked_sub(top).and_then(|j| right.get(j)).map(String::as_str).unwrap_or("");
            let _ = writeln!(out, "\x1b[2K  {art}    {text}");
        }
    } else {
        if w >= MARK_W + 4 {
            for art in mark {
                let _ = writeln!(out, "\x1b[2K  {art}");
            }
            out.push_str("\x1b[2K\n");
        }
        for l in right {
            let _ = writeln!(out, "\x1b[2K  {l}");
        }
    }
    out.push_str("\x1b[2K\n");
    out
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
    let raw = RawInput::enable();
    let started = std::time::Instant::now();
    let mut right = render(current);
    let mut drawn = false;
    let chosen = loop {
        if drawn {
            let _ = term.move_cursor_up(height(&right));
        }
        right = render(current);
        let t = if raw.is_some() { started.elapsed().as_secs_f64() } else { 0.3 };
        let _ = term.write_str(&hero_text(&right, &mark_frame(t)));
        let _ = term.clear_to_end_of_screen();
        drawn = true;
        // Keep the waves moving until a key arrives (about 15 frames a second).
        if raw.is_some() && !RawInput::wait(Duration::from_millis(66)) {
            continue;
        }
        let key = term.read_key();
        match key {
            Ok(console::Key::ArrowUp) | Ok(console::Key::Char('k')) => current = current.checked_sub(1).unwrap_or(items.len() - 1),
            Ok(console::Key::ArrowDown) | Ok(console::Key::Char('j')) | Ok(console::Key::Tab) => current = (current + 1) % items.len(),
            Ok(console::Key::Enter) => break Some(current),
            Ok(console::Key::Escape) | Ok(console::Key::Char('q' | '\u{3}')) | Ok(console::Key::CtrlC) | Err(_) => break None,
            _ => {}
        }
    };
    drop(raw);
    let _ = term.show_cursor();
    chosen
}

/// Keeps the terminal out of line mode (no echo, keys arrive at once) so the menu can wait for a
/// key with a timeout and animate meanwhile. Restores the previous mode when dropped.
pub struct RawInput(libc::termios);

impl RawInput {
    pub fn enable() -> Option<Self> {
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return None;
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(Self(saved))
        }
    }

    /// True when a key is waiting to be read. Uses select(), because macOS poll() can't wait on
    /// terminals.
    pub fn wait(timeout: Duration) -> bool {
        unsafe {
            let mut set: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&mut set);
            libc::FD_SET(libc::STDIN_FILENO, &mut set);
            let mut tv = libc::timeval {
                tv_sec: timeout.as_secs() as _,
                tv_usec: timeout.subsec_micros() as _,
            };
            libc::select(libc::STDIN_FILENO + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv) > 0
        }
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}
