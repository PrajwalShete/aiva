//! Content search (ripgrep's engine) and textual reference finding.

use super::fs_tools::PathMatcher;
use super::workspace::Resolved;
use super::{Ctx, Out, Tool, ToolError, arg_bool, arg_str, arg_usize, clip_line, req_str};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LINES_KEPT_PER_FILE: usize = 400;
const TIME_LIMIT: Duration = Duration::from_secs(20);

pub struct Line {
    pub number: u64,
    pub text: String,
    pub is_match: bool,
    /// A gap in context before this line.
    pub gap_before: bool,
}

pub struct FileHits {
    pub abs: PathBuf,
    pub display: String,
    pub mtime: SystemTime,
    pub matches: usize,
    pub lines: Vec<Line>,
}

pub struct GrepOptions {
    pub context: usize,
    pub first_only: bool,
    pub include_ignored: bool,
    pub file_filter: Option<PathMatcher>,
    pub file_type: Option<String>,
}

/// Searches every file under `bases` in parallel. Returns hits sorted by path, and whether
/// the search stopped early (time limit).
pub fn grep(ctx: &Ctx, bases: &[Resolved], matcher: &RegexMatcher, opts: &GrepOptions) -> Result<(Vec<FileHits>, bool), ToolError> {
    let types = match &opts.file_type {
        Some(t) => {
            let mut b = ignore::types::TypesBuilder::new();
            b.add_defaults();
            b.select(t);
            Some(b.build().map_err(|_| ToolError::new(format!("Unknown file type \"{t}\". Use a glob like \"*.ts\" instead.")))?)
        }
        None => None,
    };

    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for base in bases {
        let mut walker = ctx.ws.walker(base, opts.include_ignored);
        if let Some(types) = &types {
            walker.types(types.clone());
        }
        for entry in walker.build().flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if entry.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                continue;
            }
            if let Some(filter) = &opts.file_filter {
                let rel = super::workspace::slash(entry.path().strip_prefix(&base.abs).unwrap_or(entry.path()));
                if filter.matches(&rel).is_none() {
                    continue;
                }
            }
            files.push((entry.path().to_path_buf(), ctx.ws.display_abs(base, entry.path())));
        }
    }

    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(12);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut searcher = SearcherBuilder::new()
                    .binary_detection(BinaryDetection::quit(0))
                    .line_number(true)
                    .before_context(opts.context)
                    .after_context(opts.context)
                    .build();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((abs, display)) = files.get(i) else { break };
                    if started.elapsed() > TIME_LIMIT {
                        stopped.store(true, Ordering::Relaxed);
                        break;
                    }
                    let mut sink = Collect { lines: Vec::new(), matches: 0, first_only: opts.first_only, gap: false };
                    if searcher.search_path(matcher, abs, &mut sink).is_ok() && sink.matches > 0 {
                        let mtime = std::fs::metadata(abs).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                        results.lock().unwrap().push(FileHits {
                            abs: abs.clone(),
                            display: display.clone(),
                            mtime,
                            matches: sink.matches,
                            lines: sink.lines,
                        });
                    }
                }
            });
        }
    });
    let mut hits = results.into_inner().unwrap();
    hits.sort_by(|a, b| a.display.cmp(&b.display));
    Ok((hits, stopped.load(Ordering::Relaxed)))
}

struct Collect {
    lines: Vec<Line>,
    matches: usize,
    first_only: bool,
    gap: bool,
}

impl Collect {
    fn push(&mut self, number: u64, bytes: &[u8], is_match: bool) {
        if self.lines.len() >= MAX_LINES_KEPT_PER_FILE {
            return;
        }
        let text = String::from_utf8_lossy(bytes);
        let gap_before = std::mem::take(&mut self.gap);
        self.lines.push(Line { number, text: text.trim_end_matches(['\n', '\r']).to_string(), is_match, gap_before });
    }
}

impl Sink for Collect {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        let start = m.line_number().unwrap_or(0);
        for (i, line) in m.lines().enumerate() {
            self.push(start + i as u64, line, true);
            self.matches += 1;
        }
        Ok(!self.first_only)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, std::io::Error> {
        self.push(c.line_number().unwrap_or(0), c.bytes(), false);
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, std::io::Error> {
        self.gap = true;
        Ok(true)
    }
}

pub fn build_matcher(pattern: &str, fixed: bool, case_sensitive: Option<bool>) -> Result<RegexMatcher, ToolError> {
    let mut b = RegexMatcherBuilder::new();
    b.line_terminator(Some(b'\n')).fixed_strings(fixed);
    match case_sensitive {
        None => {
            b.case_smart(true);
        }
        Some(true) => {}
        Some(false) => {
            b.case_insensitive(true);
        }
    }
    b.build(pattern).map_err(|e| {
        ToolError::new(format!("Invalid regex: {e}. Escape special characters, or pass fixed_strings=true to search literally."))
    })
}

pub struct Search;

impl Tool for Search {
    fn name(&self) -> &'static str {
        "search"
    }
    fn title(&self) -> &'static str {
        "Search file contents"
    }
    fn description(&self) -> &'static str {
        "Search inside files with ripgrep's engine (Rust regex syntax; fixed_strings=true for literal text). Default output shows matching lines grouped by file as \"N: text\" (context lines as \"N- text\"); output_mode \"files\" lists matching files newest first, \"count\" gives counts per file. Smart case by default (case-insensitive unless the pattern has capitals). Narrow with path, glob (\"*.ts\", \"src/**/*.{ts,tsx}\") or type (\"rust\", \"py\", \"ts\"). Respects .gitignore."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex (or literal text with fixed_strings=true)." },
                "path": { "type": "string", "description": "Folder or file to search, e.g. \"myapp/src\". Omit for all shared folders." },
                "glob": { "type": "string", "description": "Only search files whose path matches, e.g. \"*.py\" or \"src/**/*.{ts,tsx}\"." },
                "type": { "type": "string", "description": "Only search a ripgrep file type, e.g. \"rust\", \"ts\", \"py\", \"go\", \"java\", \"md\"." },
                "output_mode": { "type": "string", "enum": ["content", "files", "count"], "description": "Default \"content\"." },
                "context": { "type": "integer", "minimum": 0, "maximum": 10, "description": "Lines of context before and after each match (content mode). Default 0." },
                "fixed_strings": { "type": "boolean", "description": "Treat pattern as literal text. Default false." },
                "case_sensitive": { "type": "boolean", "description": "Force case sensitivity on or off. Default: smart case." },
                "max_results": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Matching lines (content) or files (files/count) to return. Default 100." },
                "offset": { "type": "integer", "minimum": 0, "description": "Skip this many results (for paging). Default 0." },
                "include_ignored": { "type": "boolean", "description": "Also search files ignored by .gitignore. Default false." }
            },
            "required": ["pattern"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let pattern = req_str(args, "pattern")?;
        let mode = arg_str(args, "output_mode").unwrap_or("content");
        let max = arg_usize(args, "max_results", 100, 500).max(1);
        let offset = arg_usize(args, "offset", 0, usize::MAX);
        let context = if mode == "content" { arg_usize(args, "context", 0, 10) } else { 0 };
        let matcher = build_matcher(pattern, arg_bool(args, "fixed_strings", false), args.get("case_sensitive").and_then(Value::as_bool))?;
        let opts = GrepOptions {
            context,
            first_only: mode == "files",
            include_ignored: arg_bool(args, "include_ignored", false),
            file_filter: arg_str(args, "glob").map(PathMatcher::new).transpose()?,
            file_type: arg_str(args, "type").map(str::to_string),
        };
        let bases = ctx.ws.resolve_many(arg_str(args, "path"))?;
        let (mut hits, stopped) = grep(ctx, &bases, &matcher, &opts)?;
        let stopped_note = if stopped { " (stopped after 20s; narrow the path)" } else { "" };
        if hits.is_empty() {
            return Ok(format!(
                "No matches for /{pattern}/{stopped_note}. Try a broader pattern, case_sensitive=false, or include_ignored=true."
            ));
        }
        let total_files = hits.len();
        let mut out = Out::new();

        match mode {
            "files" => {
                hits.sort_by(|a, b| b.mtime.cmp(&a.mtime));
                out.line(&format!("{total_files} file(s) match /{pattern}/{stopped_note} (newest first):"));
                let mut last = offset;
                for h in hits.iter().skip(offset).take(max) {
                    if !out.line(&h.display) {
                        break;
                    }
                    last += 1;
                }
                let note = (last < total_files).then(|| format!("[showing {}-{last} of {total_files}; call again with offset={last}]", offset + 1));
                Ok(out.finish(note))
            }
            "count" => {
                hits.sort_by(|a, b| b.matches.cmp(&a.matches).then(a.display.cmp(&b.display)));
                let total: usize = hits.iter().map(|h| h.matches).sum();
                out.line(&format!("{total} matching lines in {total_files} file(s) for /{pattern}/{stopped_note}:"));
                let mut last = offset;
                for h in hits.iter().skip(offset).take(max) {
                    if !out.line(&format!("{}: {}", h.display, h.matches)) {
                        break;
                    }
                    last += 1;
                }
                let note = (last < total_files).then(|| format!("[showing {}-{last} of {total_files} files; call again with offset={last}]", offset + 1));
                Ok(out.finish(note))
            }
            _ => {
                let total: usize = hits.iter().map(|h| h.matches).sum();
                out.line(&format!("{total} matching lines in {total_files} file(s) for /{pattern}/{stopped_note}:"));
                let (shown, cut) = render_content(&hits, offset, max, &mut out, |_, _| String::new());
                let end = offset + shown;
                let note = if cut || end < total {
                    Some(format!("[showing matches {}-{end} of {total}; call again with offset={end}, or narrow with path/glob]", offset + 1))
                } else {
                    None
                };
                Ok(out.finish(note))
            }
        }
    }
}

/// Writes grouped content output for matches [offset, offset+max). `annotate` can add a suffix
/// to match lines. Returns (matches shown, cut short by the budget).
pub fn render_content(
    hits: &[FileHits],
    offset: usize,
    max: usize,
    out: &mut Out,
    annotate: impl Fn(&FileHits, &Line) -> String,
) -> (usize, bool) {
    let mut index = 0usize;
    let mut shown = 0usize;
    for file in hits {
        // Attach each context line to the nearest match in its block so pages stay coherent.
        let mut owner = vec![usize::MAX; file.lines.len()];
        let mut match_ids = Vec::new();
        for (i, line) in file.lines.iter().enumerate() {
            if line.is_match {
                owner[i] = index + match_ids.len();
                match_ids.push(i);
            }
        }
        let mut current = usize::MAX;
        for i in 0..file.lines.len() {
            if file.lines[i].gap_before {
                current = usize::MAX;
            }
            if file.lines[i].is_match {
                current = owner[i];
            } else if current != usize::MAX {
                owner[i] = current;
            }
        }
        let mut next = usize::MAX;
        for i in (0..file.lines.len()).rev() {
            if file.lines[i].is_match {
                next = owner[i];
            } else if owner[i] == usize::MAX {
                owner[i] = next;
            }
            if file.lines[i].gap_before {
                next = usize::MAX;
            }
        }
        let in_window = |id: usize| id != usize::MAX && id >= offset && id < offset + max;
        let mut header_done = false;
        let mut prev_shown: Option<usize> = None;
        for (i, line) in file.lines.iter().enumerate() {
            if !in_window(owner[i]) {
                continue;
            }
            if !header_done {
                if !out.line("") || !out.line(&file.display) {
                    return (shown, true);
                }
                header_done = true;
            }
            if prev_shown.is_some_and(|p| p + 1 != i || line.gap_before) && !out.line("  --") {
                return (shown, true);
            }
            let text = if line.is_match {
                format!("  {}: {}{}", line.number, clip_line(&line.text), annotate(file, line))
            } else {
                format!("  {}- {}", line.number, clip_line(&line.text))
            };
            if !out.line(&text) {
                return (shown, true);
            }
            if line.is_match {
                shown += 1;
            }
            prev_shown = Some(i);
        }
        index += match_ids.len();
        if index >= offset + max {
            break;
        }
    }
    (shown, false)
}

pub struct FindReferences;

impl Tool for FindReferences {
    fn name(&self) -> &'static str {
        "find_references"
    }
    fn title(&self) -> &'static str {
        "Find where a name is used"
    }
    fn description(&self) -> &'static str {
        "Find every use of an identifier (whole-word, case-sensitive), grouped by file, with each line labeled by the function or class it sits in and definitions marked. Text-based, so same-named things in other scopes also appear. Use find_symbol to jump to the definition itself."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Identifier, e.g. \"useAuth\" or \"UserService/login\" (only the last part is searched)." },
                "path": { "type": "string", "description": "Folder to search in. Omit for all shared folders." },
                "glob": { "type": "string", "description": "Only search matching files, e.g. \"*.ts\"." },
                "max_results": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Default 100." },
                "offset": { "type": "integer", "minimum": 0, "description": "Default 0." }
            },
            "required": ["name"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let full = req_str(args, "name")?;
        let name = full.rsplit(['/', '.', ':']).find(|s| !s.is_empty()).unwrap_or(full);
        let max = arg_usize(args, "max_results", 100, 500).max(1);
        let offset = arg_usize(args, "offset", 0, usize::MAX);
        let matcher = build_matcher(&format!(r"\b{}\b", regex::escape(name)), false, Some(true))?;
        let opts = GrepOptions {
            context: 0,
            first_only: false,
            include_ignored: false,
            file_filter: arg_str(args, "glob").map(PathMatcher::new).transpose()?,
            file_type: None,
        };
        let bases = ctx.ws.resolve_many(arg_str(args, "path"))?;
        let (hits, _) = grep(ctx, &bases, &matcher, &opts)?;
        if hits.is_empty() {
            return Ok(format!("No uses of \"{name}\" found."));
        }
        let total: usize = hits.iter().map(|h| h.matches).sum();
        let mut out = Out::new();
        out.line(&format!("\"{name}\": {total} line(s) in {} file(s) (text match):", hits.len()));
        let (shown, cut) = render_content(&hits, offset, max, &mut out, |file, line| {
            let Some(syms) = super::symbols::file_symbols(&file.abs) else { return String::new() };
            let n = line.number as usize;
            if syms.defs.iter().any(|d| d.name == name && d.start_line == n) {
                return "   [definition]".into();
            }
            syms.enclosing(n).map(|d| format!("   [in {}]", d.name)).unwrap_or_default()
        });
        let end = offset + shown;
        let note = (cut || end < total).then(|| format!("[showing {}-{end} of {total}; call again with offset={end}]", offset + 1));
        Ok(out.finish(note))
    }
}
