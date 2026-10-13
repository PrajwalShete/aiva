//! The folder picker in `aiva setup`: a search line above a checklist. Typing filters the found
//! projects and searches every folder in the home directory by name; typing a path offers that
//! folder directly. Space selects, enter confirms.

use crate::discover::FolderIndex;
use crate::ui::{self, RawInput};
use console::{Key, Term, measure_text_width, style, truncate_str};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const ACCENT: u8 = 208;
const VISIBLE: usize = 12;

pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub kind: String,
    pub when: String,
}

/// Returns the chosen folders (at least one). Ctrl+C exits setup.
pub fn folders(prompt: &str, mut rows: Vec<Row>, preselected: &[PathBuf], index: Arc<FolderIndex>) -> anyhow::Result<Vec<PathBuf>> {
    let term = Term::stdout();
    let raw = RawInput::enable();
    let _ = term.hide_cursor();
    let mut query = String::new();
    let mut selected: Vec<PathBuf> = preselected.to_vec();
    let (mut cursor, mut offset, mut drawn) = (0usize, 0usize, 0usize);
    let mut notice: Option<String> = None;
    let result = loop {
        let entries = matches(&rows, &query, &index);
        cursor = cursor.min(entries.len().saturating_sub(1));
        if cursor < offset {
            offset = cursor;
        } else if cursor >= offset + VISIBLE {
            offset = cursor + 1 - VISIBLE;
        }
        let searching = !index.done.load(Ordering::Relaxed);
        let lines = render(prompt, &query, &entries, &selected, cursor, offset, searching, notice.take());
        redraw(&term, drawn, &lines);
        drawn = lines.len();

        // While the home folder is still being indexed, refresh so new matches appear.
        if raw.is_some() && searching && !query.is_empty() && !RawInput::wait(Duration::from_millis(150)) {
            continue;
        }
        let key = match term.read_key() {
            Ok(k) => k,
            Err(e) => break Err(e.into()),
        };
        match key {
            Key::ArrowUp => cursor = cursor.checked_sub(1).unwrap_or(entries.len().saturating_sub(1)),
            Key::ArrowDown => cursor = if cursor + 1 < entries.len() { cursor + 1 } else { 0 },
            Key::PageUp => cursor = cursor.saturating_sub(VISIBLE),
            Key::PageDown => cursor = (cursor + VISIBLE).min(entries.len().saturating_sub(1)),
            Key::Char(' ') | Key::Tab => {
                if let Some(e) = entries.get(cursor) {
                    toggle(&mut selected, &mut rows, e);
                    if key == Key::Tab {
                        cursor = (cursor + 1).min(entries.len().saturating_sub(1));
                    }
                }
            }
            Key::Enter => {
                // Enter with nothing checked takes the highlighted folder.
                if selected.is_empty() {
                    if let Some(e) = entries.get(cursor) {
                        toggle(&mut selected, &mut rows, e);
                    }
                }
                if !selected.is_empty() {
                    break Ok(selected.clone());
                }
                notice = Some("Type a folder name or a path like ~/work/app, then press space.".into());
            }
            Key::Escape => query.clear(),
            Key::Backspace => {
                query.pop();
                cursor = 0;
            }
            Key::CtrlC | Key::Char('\u{3}') => {
                redraw(&term, drawn, &[]);
                drop(raw);
                let _ = term.show_cursor();
                std::process::exit(130);
            }
            Key::Char('\u{15}') => query.clear(), // ctrl+u
            Key::Char(c) if !c.is_control() => {
                query.push(c);
                cursor = 0;
            }
            _ => {}
        }
    };
    redraw(&term, drawn, &[]);
    drop(raw);
    let _ = term.show_cursor();
    result
}

struct Entry {
    path: PathBuf,
    name: String,
    kind: String,
    when: String,
}

fn toggle(selected: &mut Vec<PathBuf>, rows: &mut Vec<Row>, e: &Entry) {
    if let Some(i) = selected.iter().position(|p| *p == e.path) {
        selected.remove(i);
        return;
    }
    selected.push(e.path.clone());
    // Keep folders picked from a search in the list after the search is cleared.
    if !rows.iter().any(|r| r.path == e.path) {
        rows.push(Row { path: e.path.clone(), name: e.name.clone(), kind: e.kind.clone(), when: e.when.clone() });
    }
}

/// Found projects that match, then any other folder whose name matches, best matches first.
fn matches(rows: &[Row], query: &str, index: &FolderIndex) -> Vec<Entry> {
    let q = query.trim().to_lowercase();
    let entry = |r: &Row| Entry { path: r.path.clone(), name: r.name.clone(), kind: r.kind.clone(), when: r.when.clone() };
    if q.is_empty() {
        return rows.iter().map(entry).collect();
    }
    let mut out = Vec::new();
    // A typed path: offer that folder, then the folders in it (or beside it) that match what's
    // typed so far, so a path can be found one level at a time.
    if q.starts_with('/') || q.starts_with('~') || q.starts_with('.') {
        let typed = query.trim();
        let path = crate::config::expand_tilde(typed);
        let (dir, prefix) = if typed.ends_with('/') || path.is_dir() && typed.ends_with(['/', '~']) {
            (path.clone(), String::new())
        } else {
            let prefix = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            (path.parent().map(PathBuf::from).unwrap_or_default(), prefix)
        };
        let folder = |path: PathBuf| {
            let found = rows.iter().find(|r| r.path == path);
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| ui::tilde(&path));
            found.map(entry).unwrap_or(Entry { path, name, kind: "folder".into(), when: String::new() })
        };
        if let Ok(exact) = std::fs::canonicalize(&path) {
            if exact.is_dir() {
                out.push(folder(exact));
            }
        }
        let mut children: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().to_lowercase();
                        !name.starts_with('.') && name.starts_with(&prefix)
                    })
                    .filter_map(|e| std::fs::canonicalize(e.path()).ok())
                    .collect()
            })
            .unwrap_or_default();
        children.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        for child in children {
            if !out.iter().any(|e| e.path == child) {
                out.push(folder(child));
            }
        }
        return out;
    }
    let rank = |name: &str| -> Option<u8> {
        let name = name.to_lowercase();
        if name == q {
            Some(0)
        } else if name.starts_with(&q) {
            Some(1)
        } else if name.contains(&q) {
            Some(2)
        } else {
            None
        }
    };
    let mut known: Vec<(u8, Entry)> = rows.iter().filter_map(|r| rank(&r.name).map(|k| (k, entry(r)))).collect();
    known.sort_by_key(|(k, _)| *k);
    out.extend(known.into_iter().map(|(_, e)| e));

    let folders = index.folders.lock().unwrap();
    let mut more: Vec<(u8, usize, &PathBuf)> = folders
        .iter()
        .filter(|(p, _)| !rows.iter().any(|r| r.path == *p))
        .filter_map(|(p, name)| rank(name).map(|k| (k, p.components().count(), p)))
        .collect();
    more.sort_by_key(|(k, depth, _)| (*k, *depth));
    for (_, _, p) in more.into_iter().take(60) {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push(Entry { path: p.clone(), name, kind: "folder".into(), when: String::new() });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn render(
    prompt: &str,
    query: &str,
    entries: &[Entry],
    selected: &[PathBuf],
    cursor: usize,
    offset: usize,
    searching: bool,
    notice: Option<String>,
) -> Vec<String> {
    let accent = |s: &str| style(s.to_string()).color256(ACCENT).to_string();
    let mut lines = vec![format!(
        "  {} {}",
        style(prompt).bold(),
        ui::dim("· type to search · space to select · enter to confirm")
    )];
    let caret = style(" ").reverse().to_string();
    if query.is_empty() {
        lines.push(format!("  {}  {caret}{}", accent("⌕"), ui::dim("Search all folders by name, or type a path")));
    } else {
        lines.push(format!("  {}  {query}{caret}", accent("⌕")));
    }
    lines.push(String::new());

    let name_w = entries.iter().map(|e| e.name.chars().count()).max().unwrap_or(0).clamp(8, 24);
    let kind_w = entries.iter().map(|e| e.kind.chars().count()).max().unwrap_or(0).clamp(4, 18);
    if entries.is_empty() {
        let msg = if searching {
            "Searching your folders…"
        } else if query.starts_with(['/', '~', '.']) {
            "No folder at that path yet. Keep typing."
        } else {
            "No folders match. Try another name, or type a path like ~/work/app."
        };
        lines.push(format!("     {}", ui::dim(msg)));
    }
    if offset > 0 {
        lines.push(format!("     {}", ui::dim(format!("↑ {offset} more"))));
    }
    for (i, e) in entries.iter().enumerate().skip(offset).take(VISIBLE) {
        let checked = selected.contains(&e.path);
        let pointer = if i == cursor { accent("›") } else { " ".into() };
        let check = if checked { accent("◉") } else { ui::dim("○") };
        let name: String = e.name.chars().take(name_w).collect();
        let name = format!("{name:<name_w$}");
        let name = if i == cursor { style(name).bold().to_string() } else { name };
        lines.push(format!(
            "  {pointer} {check} {name}  {}  {}  {}",
            ui::dim(format!("{:<kind_w$}", e.kind)),
            ui::dim(format!("{:<8}", e.when)),
            ui::dim(ui::tilde(&e.path))
        ));
    }
    let below = entries.len().saturating_sub(offset + VISIBLE);
    if below > 0 {
        lines.push(format!("     {}", ui::dim(format!("↓ {below} more"))));
    }
    lines.push(String::new());
    let mut footer = match selected.len() {
        0 => "Nothing selected yet".to_string(),
        1 => "1 folder selected".to_string(),
        n => format!("{n} folders selected"),
    };
    if searching && !query.is_empty() {
        footer.push_str(" · still searching…");
    }
    lines.push(format!("  {}", ui::dim(footer)));
    if let Some(n) = notice {
        lines.push(format!("  {}", style(n).color256(ACCENT)));
    }
    lines
}

/// Replaces the previously drawn `drawn` lines with `lines`, in one write. Lines are cut to the
/// terminal width so nothing wraps and the line count stays right.
fn redraw(term: &Term, drawn: usize, lines: &[String]) {
    let width = term.size_checked().map(|(_, w)| w as usize).unwrap_or(80).saturating_sub(1);
    let mut out = String::new();
    if drawn > 0 {
        out.push_str(&format!("\x1b[{drawn}A"));
    }
    out.push_str("\r\x1b[J");
    for l in lines {
        if measure_text_width(l) > width {
            out.push_str(&truncate_str(l, width, "…"));
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    let _ = term.write_str(&out);
}
