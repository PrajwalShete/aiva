//! Tools that change files: edit_file, write_file, undo_change. Only offered while a shared
//! folder allows edits. Design (see README): Claude Code's old_string/new_string shape, tiered
//! matching in matcher.rs, all-or-nothing batches, placeholder rejection, stale-file checks,
//! a journaled backup before every write, atomic writes, and a syntax check of the result.

use super::changes;
use super::matcher::{self, Miss};
use super::{Ctx, Out, Tool, ToolError, arg_bool, arg_str, clip_line};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const MAX_FILE_BYTES: usize = 5 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const SNIPPET_CONTEXT: usize = 2;
const SNIPPET_MAX_LINES: usize = 80;

/// A text file split into what Claude edits (LF, no BOM) and the format to restore on write.
struct TextFile {
    raw: Vec<u8>,
    text: String,
    bom: bool,
    crlf: bool,
}

fn load_text(path: &Path, shown: &str) -> Result<TextFile, ToolError> {
    let raw = std::fs::read(path).map_err(|e| ToolError::io(&e, shown))?;
    if raw.len() > MAX_FILE_BYTES {
        return Err(ToolError::new(format!("{shown} is larger than 5 MB; aiva doesn't edit files that big.")));
    }
    if raw[..raw.len().min(8000)].contains(&0) {
        return Err(ToolError::new(format!("{shown} is a binary file; only text files can be edited.")));
    }
    let bom = raw.starts_with(&[0xEF, 0xBB, 0xBF]);
    let body = if bom { &raw[3..] } else { &raw[..] };
    let Ok(text) = std::str::from_utf8(body) else {
        return Err(ToolError::new(format!("{shown} isn't UTF-8 text, so editing it could corrupt characters. No changes made.")));
    };
    let crlf_count = text.matches("\r\n").count();
    let crlf = crlf_count > 0 && crlf_count * 2 >= text.matches('\n').count();
    let text = if crlf { text.replace("\r\n", "\n") } else { text.to_string() };
    Ok(TextFile { raw, text, bom, crlf })
}

fn encode(text: &str, bom: bool, crlf: bool) -> Vec<u8> {
    let body = if crlf { text.replace('\n', "\r\n") } else { text.to_string() };
    let mut out = Vec::with_capacity(body.len() + 3);
    if bom {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    out.extend_from_slice(body.as_bytes());
    out
}

fn line_count(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.trim_end_matches('\n').split('\n').count() }
}

// ---- Laziness guard -----------------------------------------------------------------------

static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?://+|#+|/\*+|\*+|<!--|--|;+|\{/\*)\s*|\s*(?:\*/\}?|-->)\s*$").unwrap());
static ELLIPSIS_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(existing|rest|unchanged|remaining|previous|omitted|same as|as before|other (code|methods|functions|props|cases|fields|imports|styles)|more (code|methods)|keep|etc)\b").unwrap()
});
static WORDY_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(\(?\s*)?((the )?rest of (the )?(code|file|function|class|component|implementation|method|module|logic)|(existing|unchanged|remaining|previous|original) (code|content|implementation|logic|methods|functions|imports|styles|props|body)( (here|goes here|remains( unchanged)?|stays the same|unchanged|as before))?)\b.*(unchanged|same|as before|remains|here|omitted|\.\.\.|…)?\s*\)?$").unwrap()
});

/// The first line of `new` that is an omission placeholder ("// ... existing code ...") not
/// already present in `context` (the old text and the original file). Returns (line, text).
fn placeholder(new: &str, context: &str) -> Option<(usize, String)> {
    for (i, line) in new.lines().enumerate() {
        let had_comment = COMMENT.is_match(line);
        let inner = COMMENT.replace_all(line, "").trim().to_string();
        if inner.is_empty() || inner.len() > 100 {
            continue;
        }
        let ellipsis = inner.contains("...") || inner.contains('…');
        let lazy = (ellipsis && ELLIPSIS_HINT.is_match(&inner)) || (had_comment && WORDY_PLACEHOLDER.is_match(&inner));
        if lazy && !context.contains(line.trim()) {
            return Some((i + 1, line.trim().to_string()));
        }
    }
    None
}

fn placeholder_error(which: &str, line: usize, text: &str) -> ToolError {
    ToolError::new(format!(
        "{which} line {line} is an omission placeholder (`{text}`). Text is written literally, so this would delete the real code. No changes made. Send the complete text, or make smaller edits that only cover the lines that change."
    ))
}

/// Claude Code strips trailing whitespace from new text except in Markdown (where it means a line break).
fn clean_new(new: &str, path: &Path) -> String {
    let markdown = path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "mdx" | "markdown"));
    let new = new.replace("\r\n", "\n");
    if markdown { new } else { new.split('\n').map(str::trim_end).collect::<Vec<_>>().join("\n") }
}

// ---- Duplicate-call protection ---------------------------------------------------------------

/// Clients sometimes retry a slow write. A repeat of the exact same call shortly after, while the
/// file still holds what that call wrote, returns the original result instead of failing.
static RECENT: LazyLock<Mutex<HashMap<String, (Instant, std::path::PathBuf, String, String)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn replayed(tool: &str, args: &Value) -> Option<String> {
    let key = format!("{tool}:{args}");
    let recent = RECENT.lock().unwrap();
    let (at, path, after, text) = recent.get(&key)?;
    let current = std::fs::read(path).ok().map(|b| changes::hash(&b));
    (at.elapsed() < Duration::from_secs(120) && current.as_deref() == Some(after.as_str()))
        .then(|| format!("{text}\n(This exact change was already applied a moment ago; nothing was done again.)"))
}

fn remember(tool: &str, args: &Value, path: &Path, after: &[u8], text: &str) {
    let mut recent = RECENT.lock().unwrap();
    recent.retain(|_, (at, ..)| at.elapsed() < Duration::from_secs(120));
    recent.insert(format!("{tool}:{args}"), (Instant::now(), path.to_path_buf(), changes::hash(after), text.to_string()));
}

// ---- Checks on the result ----------------------------------------------------------------------

/// Problems introduced by the change: invalid JSON is refused outright; new syntax errors are
/// reported so Claude fixes them in its next step.
fn check_result(path: &Path, shown: &str, before: Option<&str>, after: &str) -> Result<Vec<String>, ToolError> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if ext == "json" {
        let was_valid = before.is_none_or(|b| serde_json::from_str::<Value>(b).is_ok());
        if was_valid {
            if let Err(e) = serde_json::from_str::<Value>(after) {
                return Err(ToolError::new(format!("This would make {shown} invalid JSON ({e}). No changes made.")));
            }
        }
        return Ok(Vec::new());
    }
    let Some(after_problems) = super::symbols::syntax_problems(path, after.as_bytes()) else { return Ok(Vec::new()) };
    let before_count = before.and_then(|b| super::symbols::syntax_problems(path, b.as_bytes())).map(|p| p.len()).unwrap_or(0);
    Ok(if after_problems.len() > before_count { after_problems } else { Vec::new() })
}

fn editor_warning(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    let dir = path.parent()?;
    [format!(".#{name}"), format!(".{name}.swp")]
        .iter()
        .any(|s| dir.join(s).symlink_metadata().is_ok())
        .then(|| "Note: this file seems to be open with unsaved changes in an editor (Vim/Emacs lock file present).".to_string())
}

/// Numbered lines around each changed range of `text` (1-based, inclusive ranges).
fn snippet(text: &str, ranges: &[(usize, usize)], out: &mut Out) {
    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len().saturating_sub(usize::from(text.ends_with('\n')));
    let mut merged: Vec<(usize, usize)> = Vec::new();
    let mut sorted = ranges.to_vec();
    sorted.sort();
    for (a, b) in sorted {
        let (a, b) = (a.saturating_sub(SNIPPET_CONTEXT).max(1), (b + SNIPPET_CONTEXT).min(total.max(1)));
        match merged.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let mut shown = 0;
    for (i, (a, b)) in merged.iter().enumerate() {
        if i > 0 {
            out.line("  ...");
        }
        for n in *a..=*b {
            if shown >= SNIPPET_MAX_LINES {
                let rest: usize = merged.iter().map(|(a, b)| b + 1 - a).sum::<usize>() - shown;
                out.line(&format!("[{rest} more lines not shown; read_files to see them]"));
                return;
            }
            out.line(&format!("{n}: {}", clip_line(lines.get(n - 1).copied().unwrap_or(""))));
            shown += 1;
        }
    }
}

fn describe_ranges(ranges: &[(usize, usize)]) -> String {
    let mut sorted = ranges.to_vec();
    sorted.sort();
    sorted.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}-{b}") }).collect::<Vec<_>>().join(", ")
}

fn not_found(buf: &str, old: &str, closest: Option<(usize, usize)>, shown: &str, stale: bool) -> String {
    let mut msg = format!("old_string was not found in {shown}. No changes made.");
    if stale {
        msg.push_str(" The file changed on disk since you last read it (the user or a formatter edited it), so only exact matches are allowed.");
    }
    let lines: Vec<&str> = buf.split('\n').collect();
    if let Some((a, b)) = closest {
        let b = b.min(a + 29);
        msg.push_str(&format!("\nClosest text in the file (lines {a}-{b}):"));
        for n in a..=b {
            msg.push_str(&format!("\n{n}: {}", clip_line(lines.get(n - 1).copied().unwrap_or(""))));
        }
        let wanted: Vec<&str> = old.trim_matches('\n').split('\n').collect();
        if let Some(k) = (0..wanted.len().min(b + 1 - a)).find(|&k| wanted[k].trim_end() != lines[a - 1 + k].trim_end()) {
            let (have, want) = (lines[a - 1 + k], wanted[k]);
            if have.trim() == want.trim() {
                msg.push_str(&format!("\nFirst difference is whitespace on line {}: the file has {}, you sent {}.", a + k, ws(have), ws(want)));
            } else {
                msg.push_str(&format!("\nFirst difference is on line {}.", a + k));
            }
        }
        msg.push_str(&format!(
            "\nCopy the exact text from the file (without the \"N: \" line-number prefixes) and retry, or read_files lines {}-{} first.",
            a.saturating_sub(10).max(1),
            b + 10
        ));
    } else {
        msg.push_str(" Nothing similar was found either; read_files the part you want to change and copy old_string from it exactly.");
    }
    msg
}

fn ws(line: &str) -> String {
    let indent = &line[..line.len() - line.trim_start().len()];
    let tabs = indent.matches('\t').count();
    let spaces = indent.matches(' ').count();
    match (tabs, spaces) {
        (0, 0) => "no indentation".into(),
        (0, s) => format!("{s} leading spaces"),
        (t, 0) => format!("{t} leading tab(s)"),
        (t, s) => format!("{t} tab(s) + {s} spaces"),
    }
}

// ---- edit_file ----------------------------------------------------------------------------

struct EditSpec {
    old: String,
    new: String,
    replace_all: bool,
}

fn parse_edits(args: &Value) -> Result<Vec<EditSpec>, ToolError> {
    let one = |v: &Value| -> Result<EditSpec, ToolError> {
        let get = |a: &str, b: &str| v.get(a).or_else(|| v.get(b)).and_then(Value::as_str).map(str::to_string);
        Ok(EditSpec {
            old: get("old_string", "old_str").unwrap_or_default(),
            new: get("new_string", "new_str").ok_or_else(|| ToolError::new("Each edit needs new_string (use \"\" to delete)."))?,
            replace_all: arg_bool(v, "replace_all", false),
        })
    };
    match args.get("edits") {
        Some(Value::Array(items)) if !items.is_empty() => {
            if items.len() > 50 {
                return Err(ToolError::new("At most 50 edits per call."));
            }
            items.iter().map(one).collect()
        }
        _ => Ok(vec![one(args)?]),
    }
}

pub struct EditFile;

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }
    fn title(&self) -> &'static str {
        "Edit a file"
    }
    fn description(&self) -> &'static str {
        "Change part of a text file by replacing old_string with new_string. old_string must match the file exactly once: copy it from read_files output WITHOUT the \"N: \" prefix, keeping indentation, and add 1-3 neighboring lines if needed to make it unique (or set replace_all to change every occurrence). For several changes to one file pass edits: [{old_string, new_string}, ...] instead; they apply in order, all or nothing. new_string must be the complete code: placeholders like \"// ... existing code ...\" are rejected. An empty old_string creates a new file. The result shows the changed lines, any new syntax errors, and a change id for undo_change."
    }
    fn read_only(&self) -> bool {
        false
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to edit, e.g. \"myapp/src/server.ts\"." },
                "old_string": { "type": "string", "description": "Exact text to replace (empty to create a new file)." },
                "new_string": { "type": "string", "description": "Replacement text (empty to delete old_string)." },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence of old_string. Default false." },
                "edits": {
                    "type": "array",
                    "maxItems": 50,
                    "description": "Several edits to this file, applied in order, all or nothing. Use instead of old_string/new_string.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string" },
                            "new_string": { "type": "string" },
                            "replace_all": { "type": "boolean" }
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        if let Some(previous) = replayed(self.name(), args) {
            return Ok(previous);
        }
        let path = arg_str(args, "path").or_else(|| arg_str(args, "file_path")).ok_or_else(|| ToolError::new("Missing path."))?;
        let edits = parse_edits(args)?;
        if edits.iter().map(|e| e.old.len() + e.new.len()).sum::<usize>() > MAX_PAYLOAD_BYTES {
            return Err(ToolError::new("These edits are larger than 1 MB. Split them into smaller calls."));
        }
        let _guard = changes::lock();
        let r = ctx.ws.resolve_for_write(path)?;
        let shown = ctx.ws.display(&r);

        if !r.abs.exists() {
            if edits.len() == 1 && edits[0].old.is_empty() {
                let text = create_file(&r.abs, &shown, &edits[0].new, self.name())?;
                remember(self.name(), args, &r.abs, &std::fs::read(&r.abs).unwrap_or_default(), &text);
                return Ok(text);
            }
            return Err(ToolError::new(format!(
                "{shown} doesn't exist. To create it use write_file (or edit_file with an empty old_string); otherwise check the path with find_files."
            )));
        }

        let file = load_text(&r.abs, &shown)?;
        let stale = changes::seen(&r.abs).is_none_or(|(h, _)| h != changes::hash(&file.raw));
        let n = edits.len();
        let mut buf = file.text.clone();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        for (k, e) in edits.iter().enumerate() {
            let fail = |msg: String| {
                if n > 1 { ToolError::new(format!("Edit {}/{n} failed: {msg} None of the {n} edits were applied.", k + 1)) } else { ToolError::new(msg) }
            };
            if e.old == e.new {
                return Err(fail("old_string and new_string are identical, so there's nothing to change.".into()));
            }
            if let Some((line, text)) = placeholder(&e.new, &format!("{}\n{}", e.old, file.text)) {
                return Err(fail(placeholder_error("new_string", line, &text).0));
            }
            let new = clean_new(&e.new, &r.abs);

            if e.old.is_empty() {
                if !buf.trim().is_empty() {
                    return Err(fail(format!(
                        "old_string is empty, but {shown} already has content. Pass the exact text to replace, or use write_file to replace the whole file."
                    )));
                }
                buf = new;
                ranges = vec![(1, line_count(&buf).max(1))];
                continue;
            }

            if e.replace_all {
                let hits = matcher::exact_all(&buf, &e.old);
                if !hits.is_empty() {
                    let added = line_count(&new).max(1);
                    let mut shift: isize = 0;
                    let delta = new.matches('\n').count() as isize - e.old.matches('\n').count() as isize;
                    for h in &hits {
                        let line = (matcher::line_of(&buf, *h) as isize + shift) as usize;
                        ranges.push((line, line + added - 1));
                        shift += delta;
                    }
                    buf = buf.replace(&e.old, &new);
                    notes.push(format!("replaced {} occurrences", hits.len()));
                    continue;
                }
            }

            match matcher::find(&buf, &e.old, &new, !stale) {
                Ok(found) => {
                    let mut end = found.end;
                    // Deleting whole lines shouldn't leave an empty line behind.
                    if found.replacement.is_empty() && (found.start == 0 || buf[..found.start].ends_with('\n')) && buf[end..].starts_with('\n') {
                        end += 1;
                    }
                    let start_line = matcher::line_of(&buf, found.start);
                    let removed = buf[found.start..end].matches('\n').count() as isize;
                    let inserted = found.replacement.matches('\n').count() as isize;
                    for range in ranges.iter_mut().filter(|r| r.0 > start_line) {
                        range.0 = (range.0 as isize + inserted - removed).max(1) as usize;
                        range.1 = (range.1 as isize + inserted - removed).max(1) as usize;
                    }
                    ranges.push((start_line, start_line + line_count(&found.replacement).max(1) - 1));
                    buf.replace_range(found.start..end, &found.replacement);
                    if let Some(d) = found.strategy.describe() {
                        notes.push(d.to_string());
                    }
                }
                Err(Miss::Ambiguous { lines, strategy }) => {
                    let list = lines.iter().take(10).map(|l| l.to_string()).collect::<Vec<_>>().join(", ");
                    let how = strategy.describe().map(|d| format!(" ({d})")).unwrap_or_default();
                    return Err(fail(format!(
                        "old_string matches {} places in {shown}{how} (lines {list}). No changes made. Add 1-3 surrounding lines to make it unique, or set replace_all=true to change every one.",
                        lines.len()
                    )));
                }
                Err(Miss::NotFound { closest }) => return Err(fail(not_found(&buf, &e.old, closest, &shown, stale))),
            }
        }

        if buf == file.text {
            return Err(ToolError::new(format!("These edits leave {shown} unchanged. No changes made.")));
        }
        let problems = check_result(&r.abs, &shown, Some(&file.text), &buf)?;
        let bytes = encode(&buf, file.bom, file.crlf);
        if std::fs::read(&r.abs).ok().as_deref() != Some(&file.raw[..]) {
            return Err(ToolError::new(format!("{shown} changed on disk while this edit was being prepared. No changes made; read it again and retry.")));
        }
        let summary = format!("{} change(s) at line(s) {}", ranges.len(), describe_ranges(&ranges));
        let id = changes::record(self.name(), &summary, vec![(r.abs.clone(), shown.clone(), Some(&file.raw), Some(&bytes))])
            .map_err(|e| ToolError::new(format!("Couldn't save a backup, so nothing was changed: {e}")))?;
        if let Err(e) = changes::write(&r.abs, &bytes) {
            changes::abort(id);
            return Err(ToolError::io(&e, &shown));
        }
        changes::note_seen(&r.abs, &bytes, true);

        let mut out = Out::new();
        out.line(&format!("Edited {shown}: {summary}. Change #{id} (undo_change reverts it)."));
        notes.dedup();
        for note in &notes {
            out.line(&format!("Note: {note}."));
        }
        if stale {
            out.line("Note: the file had changed on disk since you last read it; other parts may differ from what you remember.");
        }
        if let Some(w) = editor_warning(&r.abs) {
            out.line(&w);
        }
        snippet(&buf, &ranges, &mut out);
        if !problems.is_empty() {
            out.force(&format!("Syntax check found new problems after this edit — fix them now:\n  {}", problems.join("\n  ")));
        }
        let text = out.finish(None);
        remember(self.name(), args, &r.abs, &bytes, &text);
        Ok(text)
    }
}

/// Creates a new file (the caller holds the write lock and checked it doesn't exist).
fn create_file(abs: &Path, shown: &str, content: &str, tool: &str) -> Result<String, ToolError> {
    if let Some((line, text)) = placeholder(content, "") {
        return Err(placeholder_error("content", line, &text));
    }
    let content = clean_new(content, abs);
    let problems = check_result(abs, shown, None, &content)?;
    let id = changes::record(tool, "created", vec![(abs.to_path_buf(), shown.to_string(), None, Some(content.as_bytes()))])
        .map_err(|e| ToolError::new(format!("Couldn't record the change, so nothing was written: {e}")))?;
    if let Err(e) = changes::write(abs, content.as_bytes()) {
        changes::abort(id);
        return Err(ToolError::io(&e, shown));
    }
    changes::note_seen(abs, content.as_bytes(), true);
    let mut msg = format!("Created {shown} ({} lines). Change #{id} (undo_change deletes it again).", line_count(&content));
    if !problems.is_empty() {
        msg.push_str(&format!("\nSyntax check found problems — fix them now:\n  {}", problems.join("\n  ")));
    }
    Ok(msg)
}

// ---- write_file ---------------------------------------------------------------------------

pub struct WriteFile;

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn title(&self) -> &'static str {
        "Create or replace a file"
    }
    fn description(&self) -> &'static str {
        "Create a new file, or replace an existing file's entire content. Parent folders are created as needed. To replace an existing file you must have read all of its current content first (read_files); for changes to part of a file use edit_file instead, which is safer and cheaper. content must be complete: placeholders like \"// ... rest unchanged ...\" are rejected. Returns a change id for undo_change."
    }
    fn read_only(&self) -> bool {
        false
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to create or replace, e.g. \"myapp/src/utils/date.ts\"." },
                "content": { "type": "string", "description": "The complete file content." }
            },
            "required": ["path", "content"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        if let Some(previous) = replayed(self.name(), args) {
            return Ok(previous);
        }
        let path = arg_str(args, "path").or_else(|| arg_str(args, "file_path")).ok_or_else(|| ToolError::new("Missing path."))?;
        let content = args.get("content").or_else(|| args.get("file_text")).and_then(Value::as_str).ok_or_else(|| ToolError::new("Missing content."))?;
        if content.len() > MAX_PAYLOAD_BYTES {
            return Err(ToolError::new("content is larger than 1 MB."));
        }
        let _guard = changes::lock();
        let r = ctx.ws.resolve_for_write(path)?;
        let shown = ctx.ws.display(&r);

        if !r.abs.exists() {
            let text = create_file(&r.abs, &shown, content, self.name())?;
            remember(self.name(), args, &r.abs, &std::fs::read(&r.abs).unwrap_or_default(), &text);
            return Ok(text);
        }

        let file = load_text(&r.abs, &shown)?;
        match changes::seen(&r.abs) {
            Some((h, true)) if h == changes::hash(&file.raw) => {}
            Some((h, _)) if h != changes::hash(&file.raw) => {
                return Err(ToolError::new(format!(
                    "{shown} changed on disk since you last read it (the user or a tool edited it). No changes made. Read it again with read_files, then retry — or use edit_file for a targeted change."
                )));
            }
            _ => {
                return Err(ToolError::new(format!(
                    "{shown} already exists and you haven't read all of its current content. No changes made. read_files it in full first, or use edit_file to change only part of it."
                )));
            }
        }
        if let Some((line, text)) = placeholder(content, &file.text) {
            return Err(placeholder_error("content", line, &text));
        }
        let new = clean_new(content, &r.abs);
        if new == file.text {
            return Err(ToolError::new(format!("{shown} already has exactly this content. No changes made.")));
        }
        let problems = check_result(&r.abs, &shown, Some(&file.text), &new)?;
        let bytes = encode(&new, file.bom, file.crlf);
        let (before_lines, after_lines) = (line_count(&file.text), line_count(&new));
        let id = changes::record(self.name(), &format!("rewrote ({before_lines} → {after_lines} lines)"), vec![(r.abs.clone(), shown.clone(), Some(&file.raw), Some(&bytes))])
            .map_err(|e| ToolError::new(format!("Couldn't save a backup, so nothing was changed: {e}")))?;
        if let Err(e) = changes::write(&r.abs, &bytes) {
            changes::abort(id);
            return Err(ToolError::io(&e, &shown));
        }
        changes::note_seen(&r.abs, &bytes, true);

        let mut msg = format!("Rewrote {shown} ({before_lines} → {after_lines} lines). Change #{id} (undo_change reverts it).");
        if before_lines > 20 && after_lines * 2 < before_lines {
            msg.push_str("\nWarning: the file shrank by more than half. Make sure nothing was dropped by accident; undo_change restores the old version.");
        }
        if let Some(w) = editor_warning(&r.abs) {
            msg.push_str(&format!("\n{w}"));
        }
        if !problems.is_empty() {
            msg.push_str(&format!("\nSyntax check found new problems — fix them now:\n  {}", problems.join("\n  ")));
        }
        remember(self.name(), args, &r.abs, &bytes, &msg);
        Ok(msg)
    }
}

// ---- undo_change --------------------------------------------------------------------------

pub struct UndoChange;

impl Tool for UndoChange {
    fn name(&self) -> &'static str {
        "undo_change"
    }
    fn title(&self) -> &'static str {
        "Undo a change"
    }
    fn description(&self) -> &'static str {
        "Revert a change made through aiva: the most recent one, the most recent one to a given file (path), or a specific change id from an edit result. Restores the previous content (or deletes a file that was created). Refuses if the file was modified after that change, so newer work isn't lost."
    }
    fn read_only(&self) -> bool {
        false
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Undo the latest change to this file." },
                "change_id": { "type": "integer", "description": "Undo this specific change (from an edit result)." }
            }
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let id = args.get("change_id").and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim_start_matches('#').parse().ok()));
        let only = match arg_str(args, "path") {
            Some(p) => Some(ctx.ws.resolve_for_write(p)?.abs),
            None => None,
        };
        // Only undo changes inside folders that currently allow edits.
        let allowed = |p: &Path| ctx.ws.roots.iter().any(|r| r.writable && p.starts_with(&r.path));
        if let Some(change) = changes::load().iter().rev().find(|c| {
            id.map_or(!c.undone && only.as_deref().is_none_or(|o| c.files.iter().any(|f| f.path == o)), |id| c.id == id)
        }) {
            if let Some(f) = change.files.iter().find(|f| !allowed(&f.path)) {
                return Err(ToolError::new(format!("{} is no longer in a folder that allows edits, so it can't be changed back from here.", f.display)));
            }
        }
        let change = changes::undo(id, only.as_deref())?;
        let what: Vec<String> = change
            .files
            .iter()
            .map(|f| if f.before.is_none() { format!("deleted {} (it had been created)", f.display) } else { format!("restored {}", f.display) })
            .collect();
        Ok(format!("Reverted change #{} ({}): {}.", change.id, change.tool, what.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::placeholder;

    #[test]
    fn detects_lazy_placeholders() {
        assert!(placeholder("fn a() {\n    // ... existing code ...\n}", "").is_some());
        assert!(placeholder("# rest of the file unchanged", "").is_some());
        assert!(placeholder("  /* ... */\n  {/* ... other props ... */}", "").is_some());
        assert!(placeholder("<!-- ... rest of the page ... -->", "").is_some());
    }

    #[test]
    fn allows_real_code_and_existing_placeholders() {
        assert!(placeholder("def stub(self):\n    ...\n", "").is_none());
        assert!(placeholder("// keep existing behavior for now\nlet x = 1;", "").is_none());
        assert!(placeholder("print('loading...')", "").is_none());
        assert!(placeholder("// ... existing code ...", "a\n// ... existing code ...\nb").is_none());
    }
}
