//! The screen of interactive `aiva setup`.
//!
//! Interactive setup runs full screen (the terminal's alternate screen): the animated mark stays at
//! the top while the current step is shown below it, and every frame is fitted to the window, so
//! the mark keeps moving through the whole setup without anything scrolling out of reach. Each
//! frame only rewrites the lines that changed. What the steps settled (the folders, the URL and
//! token, ...) is kept as a transcript and printed to the normal screen when setup ends or is
//! cancelled, so the scrollback reads like a plain run.
//!
//! Without a terminal the same calls print plain, static lines, exactly as before.

use crate::ui::{self, RawInput, accent, bold, dim, green};
use console::{Key, Term, measure_text_width, style, truncate_str};
use std::time::{Duration, Instant};

/// Mark sizes (columns, rows), biggest first: the screen uses the biggest that fits.
const MARKS: [(usize, usize); 3] = [(ui::MARK_W, ui::MARK_H), (28, 12), (22, 9)];

/// Marks a step header line in the section and transcript.
const STEP: char = '\u{0}';

/// About 15 frames a second.
const FRAME: Duration = Duration::from_millis(66);

pub struct Screen {
    live: Option<Live>,
    /// Lines under the brand in the hero's right column.
    status: Vec<String>,
    /// The current step, shown under the hero.
    section: Vec<String>,
    /// Everything settled so far, printed to the normal screen at the end.
    transcript: Vec<String>,
}

struct Live {
    term: Term,
    raw: Option<RawInput>,
    /// What's on screen now, one entry per row, to rewrite only what changed.
    shown: Vec<String>,
    size: (u16, u16),
}

impl Screen {
    /// Full screen when `live`, plain printed lines otherwise.
    pub fn start(live: bool) -> Self {
        let mut screen = Self { live: None, status: Vec::new(), section: Vec::new(), transcript: Vec::new() };
        if !live {
            return screen;
        }
        let term = Term::stdout();
        // The home menu leaves its hero on screen; clear it so the one printed at the end is the
        // only mark in the scrollback.
        let left = ui::take_hero_on_screen();
        if left > 0 {
            let _ = term.write_str(&format!("\x1b[{left}A\r\x1b[J"));
        }
        crate::platform::guard_terminal(b"\x1b[0m\x1b[?1049l\x1b[?25h");
        let raw = RawInput::enable();
        let _ = term.write_str("\x1b[?1049h\x1b[?25l\x1b[2J");
        screen.live = Some(Live { term, raw, shown: Vec::new(), size: (0, 0) });
        screen
    }

    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// Replaces the lines under the brand in the hero.
    pub fn set_status(&mut self, lines: Vec<String>) {
        self.status = lines;
    }

    fn hero_right(&self) -> Vec<String> {
        let mut right = ui::brand_lines("Setup");
        if !self.status.is_empty() {
            right.push(String::new());
            right.extend(self.status.iter().cloned());
        }
        right
    }

    /// The right column for the smaller marks: no divider.
    fn hero_right_short(&self) -> Vec<String> {
        let mut right = ui::brand_lines("Setup");
        right.drain(5..7);
        if !self.status.is_empty() {
            right.push(String::new());
            right.extend(self.status.iter().cloned());
        }
        right
    }

    /// Prints the hero (plain mode only; the live screen draws its own).
    pub fn banner(&self) {
        if !self.is_live() {
            self.print_hero();
        }
    }

    // ---- Settled lines ------------------------------------------------------------------

    /// Adds a settled line: printed at once in plain mode, shown under the step and kept for the
    /// transcript when live.
    pub fn line(&mut self, line: String) {
        if self.is_live() {
            // One blank line between things is enough.
            if line.is_empty() && self.transcript.last().is_some_and(String::is_empty) {
                return;
            }
            self.section.push(line.clone());
            self.transcript.push(line);
        } else {
            println!("{line}");
        }
    }

    pub fn ok(&mut self, msg: impl std::fmt::Display) {
        self.line(format!("  {} {msg}", style("✓").green()));
    }

    pub fn warn(&mut self, msg: impl std::fmt::Display) {
        self.line(format!("  {} {msg}", style("!").yellow().bold()));
    }

    pub fn fail(&mut self, msg: impl std::fmt::Display) {
        if self.is_live() {
            self.line(format!("  {} {msg}", style("✗").red().bold()));
        } else {
            ui::fail(msg);
        }
    }

    pub fn info(&mut self, msg: impl std::fmt::Display) {
        self.line(format!("  {msg}"));
    }

    pub fn blank(&mut self) {
        self.line(String::new());
    }

    pub fn card(&mut self, title: &str, lines: &[String]) {
        for l in ui::card_lines(title, lines) {
            self.line(l);
        }
    }

    /// Starts a step: the live screen shows only the current step under the hero.
    pub fn step(&mut self, n: usize, total: usize, title: &str) {
        if self.is_live() {
            // The rule is drawn to the window's width when shown (see `wrap`), so it follows resizes.
            let rule = format!("{STEP}{n}{STEP}{total}{STEP}{title}");
            // The hero already ends with a blank line.
            self.section = vec![rule.clone(), String::new()];
            self.transcript.extend([String::new(), rule, String::new()]);
        } else {
            for l in ui::step_lines(n, total, title) {
                println!("{l}");
            }
        }
    }

    // ---- Drawing --------------------------------------------------------------------------

    /// Draws one frame: the hero, the current step, then `widget(rows)`, which must return at most
    /// `rows` lines and needs `min` to be usable. When the mark doesn't fit above that, the hero
    /// shrinks to a one-line brand.
    pub fn draw(&mut self, min: usize, widget: &dyn Fn(usize) -> Vec<String>) {
        let Some(live) = self.live.as_ref() else { return };
        let (rows, cols) = live.term.size_checked().unwrap_or((24, 80));
        let (rows, cols) = (rows as usize, cols as usize);
        let section: Vec<String> = self.section.iter().flat_map(|l| wrap(l, cols.saturating_sub(1))).collect();
        let section_lines = section.len();
        let mut frame = Vec::with_capacity(rows);
        // The biggest mark that leaves the widget its room, beside the brand column.
        let (full, short) = (self.hero_right(), self.hero_right_short());
        let hero = MARKS.iter().find_map(|&(mark_w, mark_h)| {
            let right = if full.len() <= mark_h { &full } else { &short };
            let right_w = right.iter().map(|l| measure_text_width(l)).max().unwrap_or(0);
            let height = mark_h.max(right.len()) + 2;
            (cols >= mark_w + 6 + right_w && height + section_lines + min <= rows).then_some((mark_w, mark_h, right))
        });
        if let Some((mark_w, mark_h, right)) = hero {
            frame.extend(ui::hero_beside(right, &ui::mark_sized(ui::clock(), mark_w, mark_h), mark_w));
        } else {
            frame.push(String::new());
            frame.push(format!(
                "  {}  {}",
                style("aiva").color256(208).bold(),
                dim(format!("Setup · v{}", crate::VERSION))
            ));
            frame.push(String::new());
        }
        // Keep the step header and the newest settled lines when the step is too tall.
        let room = rows.saturating_sub(frame.len() + min);
        if section_lines <= room {
            frame.extend(section);
        } else if room > 0 {
            frame.push(section[0].clone());
            frame.extend(section[section_lines + 1 - room..].iter().cloned());
        }
        let avail = rows.saturating_sub(frame.len());
        let mut lines = widget(avail);
        lines.truncate(avail);
        frame.extend(lines);
        self.paint(frame, (rows as u16, cols as u16));
    }

    /// Writes the frame, only the rows that changed, in one synchronized update.
    fn paint(&mut self, frame: Vec<String>, size: (u16, u16)) {
        let Some(live) = self.live.as_mut() else { return };
        let width = (size.1 as usize).saturating_sub(1);
        let frame: Vec<String> = frame
            .into_iter()
            .map(|l| if measure_text_width(&l) > width { truncate_str(&l, width, "…").into_owned() } else { l })
            .collect();
        let mut out = String::from("\x1b[?2026h");
        if size != live.size {
            out.push_str("\x1b[H\x1b[2J");
            live.shown.clear();
            live.size = size;
        }
        for i in 0..frame.len().max(live.shown.len()) {
            let new = frame.get(i).map(String::as_str).unwrap_or("");
            if live.shown.get(i).map(String::as_str) != Some(new) {
                out.push_str(&format!("\x1b[{};1H{new}\x1b[0m\x1b[K", i + 1));
            }
        }
        out.push_str("\x1b[?2026l");
        let _ = live.term.write_str(&out);
        live.shown = frame;
    }

    /// Animates until a key arrives (returned) or `timeout` passes (None). Ctrl+C ends setup.
    pub fn poll(&mut self, min: usize, widget: &dyn Fn(usize) -> Vec<String>, timeout: Option<Duration>) -> Option<Key> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            self.draw(min, widget);
            let wait = deadline.map(|d| d.saturating_duration_since(Instant::now()).min(FRAME)).unwrap_or(FRAME);
            if RawInput::wait(wait) {
                let key = self.live.as_ref().map(|l| l.term.read_key_raw());
                match key {
                    Some(Ok(Key::CtrlC | Key::Char('\u{3}'))) | Some(Err(_)) | None => self.cancel(),
                    Some(Ok(key)) => return Some(key),
                }
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return None;
            }
        }
    }

    /// Waits for a key (animating meanwhile).
    pub fn key(&mut self, min: usize, widget: &dyn Fn(usize) -> Vec<String>) -> Key {
        loop {
            if let Some(key) = self.poll(min, widget, None) {
                return key;
            }
        }
    }

    // ---- Widgets ----------------------------------------------------------------------------

    /// Runs `work` on another thread with a spinner line until it's done.
    pub fn work<T: Send + 'static>(&mut self, label: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
        if !self.is_live() {
            let pb = ui::spinner(label.to_string());
            let out = work();
            pb.finish_and_clear();
            return out;
        }
        let worker = std::thread::spawn(work);
        let line = |_: usize| vec![format!("  {} {label}", ui::spin())];
        while !worker.is_finished() {
            self.poll(1, &line, Some(FRAME));
        }
        worker.join().expect("setup worker")
    }

    /// A spinner line until `check` returns true (then true) or `limit` passes or the person
    /// presses esc, enter or q (then false). `check` runs about every 400 ms.
    pub fn wait_until(&mut self, label: &str, limit: Duration, mut check: impl FnMut() -> bool) -> bool {
        let started = Instant::now();
        if !self.is_live() {
            return plain_wait(label, limit, check);
        }
        ui::flush_input();
        let line = |_: usize| vec![format!("  {} {label}", ui::spin())];
        let mut next = Instant::now();
        while started.elapsed() < limit {
            if Instant::now() >= next {
                if check() {
                    return true;
                }
                next = Instant::now() + Duration::from_millis(400);
            }
            if let Some(Key::Escape | Key::Enter | Key::Char('q')) = self.poll(1, &line, Some(Duration::from_millis(400))) {
                return false;
            }
        }
        false
    }

    /// A yes/no question. Settles as `✓ prompt · yes`.
    pub fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool> {
        if !self.is_live() {
            return Ok(dialoguer::Confirm::with_theme(&ui::theme()).with_prompt(prompt).default(default).interact()?);
        }
        ui::flush_input();
        let mut yes = default;
        let answer = loop {
            let render = |_: usize| {
                let (y, n) = if yes { (accent(bold("Yes")), dim("No")) } else { (dim("Yes"), accent(bold("No"))) };
                vec![
                    format!("  {} {}  {y}  {n}", bold(prompt), dim("›")),
                    String::new(),
                    format!("  {}", dim("←→ to choose · y/n · enter to confirm")),
                ]
            };
            match self.key(3, &render) {
                Key::ArrowLeft | Key::ArrowRight | Key::Tab | Key::ArrowUp | Key::ArrowDown => yes = !yes,
                Key::Char('y' | 'Y') => break true,
                Key::Char('n' | 'N') | Key::Escape => break false,
                Key::Enter => break yes,
                _ => {}
            }
        };
        self.line(format!("  {} {} {} {}", green("✓"), prompt, dim("·"), accent(if answer { "yes" } else { "no" })));
        Ok(answer)
    }

    /// `Enter to copy the URL · esc to skip`. True on enter.
    pub fn press_enter(&mut self, prompt: &str) -> bool {
        if !self.is_live() {
            ui::info(prompt);
            return ui::wait_enter();
        }
        ui::flush_input();
        let render = |_: usize| vec![format!("  {prompt}")];
        loop {
            match self.key(1, &render) {
                Key::Enter => return true,
                Key::Escape | Key::Char('q') => return false,
                _ => {}
            }
        }
    }

    /// A checklist. Returns the checked indexes; esc leaves everything unchecked.
    pub fn multi_select(&mut self, prompt: &str, items: &[String], defaults: &[bool]) -> anyhow::Result<Vec<usize>> {
        if !self.is_live() {
            return Ok(dialoguer::MultiSelect::with_theme(&ui::theme())
                .with_prompt(prompt)
                .items(items)
                .defaults(defaults)
                .report(false)
                .interact_opt()?
                .unwrap_or_default());
        }
        ui::flush_input();
        let mut checked: Vec<bool> = (0..items.len()).map(|i| defaults.get(i).copied().unwrap_or(false)).collect();
        let mut cursor = 0usize;
        let offset = std::cell::Cell::new(0usize);
        let shown = std::cell::Cell::new(items.len());
        // header, blank, rows, ↑/↓ more, blank, footer
        let min = 4 + items.len().min(3) + 2;
        loop {
            let render = |avail: usize| {
                let mut lines = vec![format!("  {} {}", bold(prompt), dim("· space to select · enter to confirm")), String::new()];
                let rows = if items.len() + 3 <= avail { items.len() } else { avail.saturating_sub(5).max(1) };
                shown.set(rows);
                let mut off = offset.get().min(items.len().saturating_sub(rows));
                if cursor < off {
                    off = cursor;
                } else if cursor >= off + rows {
                    off = cursor + 1 - rows;
                }
                offset.set(off);
                if off > 0 {
                    lines.push(format!("     {}", dim(format!("↑ {off} more"))));
                }
                for (i, item) in items.iter().enumerate().skip(off).take(rows) {
                    let pointer = if i == cursor { accent("›") } else { " ".into() };
                    let check = if checked[i] { accent("◉") } else { dim("○") };
                    let item = if i == cursor { bold(item) } else { item.clone() };
                    lines.push(format!("  {pointer} {check} {item}"));
                }
                let below = items.len().saturating_sub(off + rows);
                if below > 0 {
                    lines.push(format!("     {}", dim(format!("↓ {below} more"))));
                }
                lines.push(String::new());
                let n = checked.iter().filter(|c| **c).count();
                lines.push(format!("  {}", dim(if n == 0 { "Read-only everywhere".to_string() } else { format!("Edits allowed in {n} of {}", items.len()) })));
                lines
            };
            match self.key(min, &render) {
                Key::ArrowUp | Key::Char('k') => cursor = cursor.checked_sub(1).unwrap_or(items.len().saturating_sub(1)),
                Key::ArrowDown | Key::Char('j') => cursor = if cursor + 1 < items.len() { cursor + 1 } else { 0 },
                Key::PageUp => cursor = cursor.saturating_sub(shown.get()),
                Key::PageDown => cursor = (cursor + shown.get()).min(items.len().saturating_sub(1)),
                Key::Char(' ') | Key::Tab => {
                    if let Some(c) = checked.get_mut(cursor) {
                        *c = !*c;
                    }
                }
                Key::Char('a') => {
                    let all = checked.iter().all(|c| *c);
                    checked.iter_mut().for_each(|c| *c = !all);
                }
                Key::Enter => return Ok((0..items.len()).filter(|i| checked[*i]).collect()),
                Key::Escape => return Ok(Vec::new()),
                _ => {}
            }
        }
    }

    // ---- Ending -----------------------------------------------------------------------------

    /// Leaves the full screen and prints the hero and the transcript to the normal screen.
    pub fn finish(&mut self) {
        let Some(live) = self.live.take() else { return };
        let _ = live.term.write_str("\x1b[?2026l\x1b[0m\x1b[?1049l\x1b[?25h");
        drop(live.raw);
        self.print_hero();
        // The hero already ends with a blank line.
        if self.transcript.first().is_some_and(String::is_empty) {
            self.transcript.remove(0);
        }
        let width = Term::stdout().size_checked().map(|(_, w)| w as usize).unwrap_or(80).saturating_sub(1);
        for l in self.transcript.drain(..) {
            for l in wrap(&l, width) {
                println!("{l}");
            }
        }
    }

    /// The still hero for the normal screen: the biggest mark that fits beside the brand column.
    fn print_hero(&self) {
        let cols = Term::stdout().size_checked().map(|(_, w)| w as usize).unwrap_or(80);
        let (full, short) = (self.hero_right(), self.hero_right_short());
        for &(mark_w, mark_h) in &MARKS {
            let right = if full.len() <= mark_h { &full } else { &short };
            let right_w = right.iter().map(|l| measure_text_width(l)).max().unwrap_or(0);
            if cols >= mark_w + 6 + right_w {
                for l in ui::hero_beside(right, &ui::mark_sized(0.3, mark_w, mark_h), mark_w) {
                    println!("{l}");
                }
                return;
            }
        }
        ui::hero(&full);
    }

    /// Ctrl+C: restore the terminal, keep what was settled, and exit.
    fn cancel(&mut self) -> ! {
        if self.is_live() {
            self.blank();
            self.warn("Setup cancelled.");
            self.blank();
        }
        self.finish();
        std::process::exit(130);
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Wraps a long line at spaces to `width` columns, continuing under its text.
fn wrap(line: &str, width: usize) -> Vec<String> {
    if let Some(step) = line.strip_prefix(STEP) {
        let mut parts = step.splitn(3, STEP);
        let n = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let total = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        return vec![ui::step_lines(n, total, parts.next().unwrap_or(""))[1].clone()];
    }
    if measure_text_width(line) <= width || width < 30 {
        return vec![line.to_string()];
    }
    let lead = line.len() - line.trim_start().len();
    // Continue under the text after a leading mark such as "✓" or "!".
    let first = line.trim_start().split(' ').next().unwrap_or("");
    let indent = if measure_text_width(first) == 1 { lead + 2 } else { lead };
    let mut out = Vec::new();
    let mut cur = " ".repeat(lead);
    let mut fresh = true;
    for word in line.trim_start().split(' ') {
        if !fresh && measure_text_width(&cur) + 1 + measure_text_width(word) > width {
            // Carry the colors still open at the break onto the next line.
            let open = open_styles(&cur);
            if !open.is_empty() {
                cur.push_str("\x1b[0m");
            }
            out.push(std::mem::replace(&mut cur, format!("{}{open}", " ".repeat(indent))));
            fresh = true;
        }
        if !fresh {
            cur.push(' ');
        }
        cur.push_str(word);
        fresh = false;
    }
    out.push(cur);
    out
}

/// The SGR sequences (colors, bold, dim) still in effect at the end of `s`.
fn open_styles(s: &str) -> String {
    let mut open = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("\x1b[") {
        let seq = &rest[i..];
        let Some(end) = seq.find(|c: char| c.is_ascii_alphabetic()) else { break };
        if seq.as_bytes()[end] == b'm' {
            if seq[2..end].is_empty() || &seq[2..end] == "0" {
                open.clear();
            } else {
                open.push_str(&seq[..=end]);
            }
        }
        rest = &seq[end + 1..];
    }
    open
}

/// The plain (non full screen) wait: a spinner and a key reader on another thread.
fn plain_wait(label: &str, limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let pb = ui::spinner(label.to_string());
    let skipped = Arc::new(AtomicBool::new(false));
    {
        let skipped = skipped.clone();
        std::thread::spawn(move || {
            let term = Term::stderr();
            while let Ok(key) = term.read_key() {
                if matches!(key, Key::Escape | Key::Enter | Key::Char('q')) {
                    skipped.store(true, Ordering::Relaxed);
                    return;
                }
            }
        });
    }
    let started = Instant::now();
    let mut seen = false;
    while started.elapsed() < limit && !skipped.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(400));
        if check() {
            seen = true;
            break;
        }
    }
    pb.finish_and_clear();
    seen
}
