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

/// The step header as lines: a blank, `── 1 of 3 · Choose folders ──────────`, a blank.
pub fn step_lines(n: usize, total: usize, title: &str) -> Vec<String> {
    let label = format!(" {n} of {total} · {title} ");
    let line = width().min(72) - 4;
    let rule = "─".repeat(line.saturating_sub(2 + measure_text_width(&label)));
    vec![String::new(), format!("  {}{}{}", dim("──"), style(label).bold(), dim(rule)), String::new()]
}

/// A rounded box around `lines` (already styled). Falls back to plain lines on narrow terminals.
pub fn card(title: &str, lines: &[String]) {
    for l in card_lines(title, lines) {
        println!("{l}");
    }
}

pub fn card_lines(title: &str, lines: &[String]) -> Vec<String> {
    let inner = lines.iter().map(|l| measure_text_width(l)).chain([measure_text_width(title)]).max().unwrap_or(0) + 4;
    if inner + 4 > width() {
        return std::iter::once(format!("  {}", bold(title))).chain(lines.iter().map(|l| format!("  {l}"))).collect();
    }
    let pad = |s: &str| format!("{s}{}", " ".repeat(inner.saturating_sub(measure_text_width(s) + 2)));
    let mut out = vec![format!("  {}", dim(format!("╭{}╮", "─".repeat(inner))))];
    out.push(format!("  {} {} {}", dim("│"), pad(&bold(title)), dim("│")));
    out.push(format!("  {} {} {}", dim("│"), pad(""), dim("│")));
    for l in lines {
        out.push(format!("  {} {} {}", dim("│"), pad(l), dim("│")));
    }
    out.push(format!("  {}", dim(format!("╰{}╯", "─".repeat(inner)))));
    out
}

/// A spinner line (for plain, non full screen output).
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
            return if rest.as_os_str().is_empty() { "~".into() } else { format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()) };
        }
    }
    path.display().to_string()
}

pub use crate::platform::{RawInput, copy, flush_input, open_url};

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

pub const MARK_W: usize = 40;
pub const MARK_H: usize = 17;

/// The aiva mark drawn in text: the brand dot eclipsed into a dark disc, haloed by an orange
/// corona with a bright flare at the lower right. Computed, so it stays crisp and consistent.
pub fn mark() -> Vec<String> {
    mark_frame(0.3)
}

/// One frame of the mark at `t` seconds: rings of light ripple outward through the corona like
/// sound waves, the streamers drift slowly and the flare breathes.
pub fn mark_frame(t: f64) -> Vec<String> {
    mark_sized(t, MARK_W, MARK_H)
}

/// The mark drawn `w` by `h` cells (the full mark is 40 by 17).
pub fn mark_sized(t: f64, w: usize, h: usize) -> Vec<String> {
    const RAMP: &[u8] = b" .:-=+*#%@";
    let scale = MARK_H as f64 / h as f64;
    let sample = |i: f64, j: f64| -> f64 {
        let x = (i - (w as f64 - 1.0) / 2.0) / 2.0 * scale;
        let y = (j - (h as f64 - 1.0) / 2.0) * scale;
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
    let mut lines = Vec::with_capacity(h);
    for j in 0..h {
        let mut line = String::new();
        for i in 0..w {
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

/// One clock for every animated mark, so the waves don't jump when one screen hands over to the
/// next (the home menu to setup).
pub fn clock() -> f64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
}

/// Lines of a hero still at the bottom of the screen (left by the home menu), which setup clears
/// before it draws its own, so there is only ever one mark.
static HERO_ON_SCREEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Takes the height of the hero the home menu left on screen (0 if none).
pub fn take_hero_on_screen() -> usize {
    HERO_ON_SCREEN.swap(0, std::sync::atomic::Ordering::Relaxed)
}

/// A spinner glyph for animated text, in step with the mark.
pub fn spin() -> String {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    style(FRAMES[(clock() * 12.0) as usize % FRAMES.len()]).color256(ACCENT).to_string()
}

/// The mark on the left and `right` (already styled) on the right, vertically centered.
/// Stacks them on narrow terminals and drops the mark when there's no room.
pub fn hero(right: &[String]) {
    print!("{}", hero_text(right, &mark()));
}

/// The hero as text, every line starting with "clear line" so it can be redrawn in place.
fn hero_text(right: &[String], mark: &[String]) -> String {
    let w = Term::stdout().size_checked().map(|(_, w)| w as usize).unwrap_or(80);
    hero_lines(right, mark, w).iter().map(|l| format!("\x1b[2K{l}\n")).collect()
}

/// How the hero fits `w` columns: the mark beside the right column, the mark above it, or no mark.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeroFit {
    Beside,
    Above,
    NoMark,
}

pub fn hero_fit(right: &[String], w: usize) -> HeroFit {
    let right_w = right.iter().map(|l| measure_text_width(l)).max().unwrap_or(0);
    if w >= MARK_W + 6 + right_w {
        HeroFit::Beside
    } else if w >= MARK_W + 4 {
        HeroFit::Above
    } else {
        HeroFit::NoMark
    }
}

/// Number of lines `hero_lines` returns.
pub fn hero_height(right: &[String], w: usize) -> usize {
    match hero_fit(right, w) {
        HeroFit::Beside => MARK_H.max(right.len()) + 2,
        HeroFit::Above => MARK_H + right.len() + 3,
        HeroFit::NoMark => right.len() + 2,
    }
}

/// The hero as lines (a blank above and below): the mark on the left and `right` (already
/// styled) beside it, vertically centered. Stacks them on narrow terminals and drops the mark
/// when there's no room.
pub fn hero_lines(right: &[String], mark: &[String], w: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    match hero_fit(right, w) {
        HeroFit::Beside => return hero_beside(right, mark, MARK_W),
        HeroFit::Above => {
            out.extend(mark.iter().map(|art| format!("  {art}")));
            out.push(String::new());
            out.extend(right.iter().map(|l| format!("  {l}")));
        }
        HeroFit::NoMark => out.extend(right.iter().map(|l| format!("  {l}"))),
    }
    out.push(String::new());
    out
}

/// The mark (`mark_w` columns wide) with `right` beside it, vertically centered, and a blank line
/// above and below.
pub fn hero_beside(right: &[String], mark: &[String], mark_w: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    let top = mark.len().saturating_sub(right.len()) / 2;
    let empty = " ".repeat(mark_w);
    for i in 0..mark.len().max(right.len()) {
        let art = mark.get(i).map(String::as_str).unwrap_or(&empty);
        let text = i.checked_sub(top).and_then(|j| right.get(j)).map(String::as_str).unwrap_or("");
        out.push(format!("  {art}    {text}"));
    }
    out.push(String::new());
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
    let height = |right: &[String]| hero_height(right, term.size_checked().map(|(_, w)| w as usize).unwrap_or(80));
    let _ = term.hide_cursor();
    let raw = RawInput::enable();
    let mut right = render(current);
    let mut drawn = false;
    let chosen = loop {
        if drawn {
            let _ = term.move_cursor_up(height(&right));
        }
        right = render(current);
        let t = if raw.is_some() { clock() } else { 0.3 };
        let _ = term.write_str(&hero_text(&right, &mark_frame(t)));
        let _ = term.clear_to_end_of_screen();
        drawn = true;
        // Keep the waves moving until a key arrives (about 15 frames a second).
        if raw.is_some() && !RawInput::wait(Duration::from_millis(66)) {
            continue;
        }
        let key = term.read_key_raw();
        match key {
            Ok(console::Key::ArrowUp) | Ok(console::Key::Char('k')) => current = current.checked_sub(1).unwrap_or(items.len() - 1),
            Ok(console::Key::ArrowDown) | Ok(console::Key::Char('j')) | Ok(console::Key::Tab) => current = (current + 1) % items.len(),
            Ok(console::Key::Enter) => break Some(current),
            Ok(console::Key::Escape) | Ok(console::Key::Char('q' | '\u{3}')) | Ok(console::Key::CtrlC) | Err(_) => break None,
            _ => {}
        }
    };
    drop(raw);
    if chosen.is_some() {
        HERO_ON_SCREEN.store(height(&right), std::sync::atomic::Ordering::Relaxed);
    }
    let _ = term.show_cursor();
    chosen
}


