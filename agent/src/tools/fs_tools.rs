//! Filesystem browsing: list_folders, list_dir, find_files, read_files.

use super::workspace::Resolved;
use super::{Ctx, Out, Tool, ToolError, ago, arg_bool, arg_str, arg_usize, clip_line, human_size, read_text, req_str};
use globset::{GlobBuilder, GlobMatcher};
use serde_json::{Value, json};
use std::time::SystemTime;

pub struct ListFolders;

impl Tool for ListFolders {
    fn name(&self) -> &'static str {
        "list_folders"
    }
    fn title(&self) -> &'static str {
        "List shared folders"
    }
    fn description(&self) -> &'static str {
        "List the folders the user shared with you and whether each allows edits. Every path you pass to other tools starts with one of these folder names, e.g. \"myapp/src/main.ts\". Call this first if you don't know the folder names yet."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn run(&self, _args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        if ctx.ws.roots.is_empty() {
            return Err(ToolError::new("No folders are shared yet. The user can add one with `aiva folders add <path>`."));
        }
        let mut out = String::from("Shared folders:\n");
        for r in &ctx.ws.roots {
            let access = if r.writable { "read + write" } else { "read-only" };
            out.push_str(&format!("- {}  ({})  {access}\n", r.label, crate::ui::tilde(&r.path)));
        }
        out.push_str(&format!("Paths start with the folder name, e.g. \"{}/README.md\".", ctx.ws.roots[0].label));
        Ok(out)
    }
}

pub struct ListDir;

impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "list_dir"
    }
    fn title(&self) -> &'static str {
        "List a folder as a tree"
    }
    fn description(&self) -> &'static str {
        "Show the files and subfolders under a path as an indented tree (folders end with /, folders past the depth limit show their item count). Respects .gitignore and skips .git and node_modules unless include_ignored is true. Omit path to list every shared folder. For a first look at a project prefer project_overview, which includes a tree plus languages, manifests and key symbols."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Folder to list, e.g. \"myapp/src\". Omit for all shared folders." },
                "depth": { "type": "integer", "description": "How many levels deep to show (1-8). Default 2.", "minimum": 1, "maximum": 8 },
                "max_entries": { "type": "integer", "description": "Maximum entries to show. Default 200.", "minimum": 1, "maximum": 1000 },
                "show_sizes": { "type": "boolean", "description": "Show file sizes. Default false." },
                "include_ignored": { "type": "boolean", "description": "Also show files ignored by .gitignore. Default false." }
            }
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let depth = arg_usize(args, "depth", 2, 8).max(1);
        let max_entries = arg_usize(args, "max_entries", 200, 1000).max(1);
        let sizes = arg_bool(args, "show_sizes", false);
        let ignored = arg_bool(args, "include_ignored", false);
        let mut out = Out::new();
        let mut shown = 0;
        let mut total = 0;
        for base in ctx.ws.resolve_many(arg_str(args, "path"))? {
            if base.abs.is_file() {
                return Err(ToolError::new(format!("{} is a file. Use read_files to read it.", ctx.ws.display(&base))));
            }
            let (s, t) = tree(ctx, &base, depth, max_entries.saturating_sub(shown), sizes, ignored, &mut out);
            shown += s;
            total += t;
        }
        let note = (total > shown || out.full).then(|| {
            format!("[showing {shown} of {total}+ entries; list a subfolder or lower depth to see the rest]")
        });
        Ok(out.finish(note))
    }
}

/// Renders one tree into `out`. Returns (entries shown, entries seen).
pub fn tree(ctx: &Ctx, base: &Resolved, depth: usize, max: usize, sizes: bool, ignored: bool, out: &mut Out) -> (usize, usize) {
    out.line(&format!("{}/", ctx.ws.display(base)));
    let mut walker = ctx.ws.walker(base, ignored);
    walker.max_depth(Some(depth)).sort_by_file_path(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.file_name().cmp(&b.file_name())));
    let (mut shown, mut seen) = (0, 0);
    for entry in walker.build().flatten() {
        if entry.depth() == 0 {
            continue;
        }
        seen += 1;
        if shown >= max || out.full {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let indent = "  ".repeat(entry.depth());
        let ft = entry.file_type();
        let line = if entry.path_is_symlink() {
            format!("{indent}{name}@")
        } else if ft.is_some_and(|t| t.is_dir()) {
            if entry.depth() == depth {
                let count = std::fs::read_dir(entry.path()).map(|d| d.count()).unwrap_or(0);
                format!("{indent}{name}/  ({count} items)")
            } else {
                format!("{indent}{name}/")
            }
        } else if sizes {
            let size = entry.metadata().map(|m| human_size(m.len())).unwrap_or_default();
            format!("{indent}{name}  {size}")
        } else {
            format!("{indent}{name}")
        };
        if out.line(&line) {
            shown += 1;
        }
    }
    (shown, seen)
}

pub struct FindFiles;

impl Tool for FindFiles {
    fn name(&self) -> &'static str {
        "find_files"
    }
    fn title(&self) -> &'static str {
        "Find files by name"
    }
    fn description(&self) -> &'static str {
        "Find files whose path matches a glob (\"*.tsx\", \"src/**/*test*\", \"*.{yml,yaml}\") or, without glob characters, contains the text (case-insensitive, e.g. \"auth\" finds src/lib/auth.ts). Patterns without a slash match at any depth. Results are newest first so recently edited files surface; respects .gitignore. Use search to look inside files instead."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob or plain text to match against file paths." },
                "path": { "type": "string", "description": "Folder to search in, e.g. \"myapp/src\". Omit for all shared folders." },
                "max_results": { "type": "integer", "description": "Default 100.", "minimum": 1, "maximum": 500 },
                "offset": { "type": "integer", "description": "Skip this many results (for paging). Default 0.", "minimum": 0 },
                "include_ignored": { "type": "boolean", "description": "Also match files ignored by .gitignore. Default false." }
            },
            "required": ["pattern"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let pattern = req_str(args, "pattern")?;
        let max = arg_usize(args, "max_results", 100, 500).max(1);
        let offset = arg_usize(args, "offset", 0, usize::MAX);
        let ignored = arg_bool(args, "include_ignored", false);
        let matcher = PathMatcher::new(pattern)?;

        let mut hits: Vec<(String, SystemTime, bool)> = Vec::new();
        for base in ctx.ws.resolve_many(arg_str(args, "path"))? {
            for entry in ctx.ws.walker(&base, ignored).build().flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let rel = super::workspace::slash(entry.path().strip_prefix(&base.abs).unwrap_or(entry.path()));
                if let Some(name_hit) = matcher.matches(&rel) {
                    let mtime = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
                    hits.push((ctx.ws.display_abs(&base, entry.path()), mtime, name_hit));
                }
            }
        }
        if hits.is_empty() {
            return Ok(format!("No files match \"{pattern}\". Try a broader pattern, or include_ignored=true for build output and dependencies."));
        }
        // File-name hits first (for plain-text patterns), then newest first.
        hits.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        let total = hits.len();
        let mut out = Out::new();
        out.line(&format!("{total} file(s) match \"{pattern}\" (newest first):"));
        let mut last = offset;
        for (display, mtime, _) in hits.iter().skip(offset).take(max) {
            if !out.line(&format!("{display}  ({})", ago(*mtime))) {
                break;
            }
            last += 1;
        }
        let note = (last < total).then(|| format!("[showing {}-{last} of {total}; call again with offset={last}]", offset + 1));
        Ok(out.finish(note))
    }
}

/// Glob or substring matching against `/`-separated relative paths.
pub struct PathMatcher {
    glob: Option<GlobMatcher>,
    needle: String,
}

impl PathMatcher {
    pub fn new(pattern: &str) -> Result<PathMatcher, ToolError> {
        let pattern = pattern.trim().trim_start_matches("./");
        if pattern.contains(['*', '?', '[', '{']) {
            let full = if pattern.contains('/') { pattern.to_string() } else { format!("**/{pattern}") };
            let glob = GlobBuilder::new(&full)
                .case_insensitive(true)
                .literal_separator(true)
                .build()
                .map_err(|e| ToolError::new(format!("Invalid glob \"{pattern}\": {e}")))?;
            Ok(PathMatcher { glob: Some(glob.compile_matcher()), needle: String::new() })
        } else {
            Ok(PathMatcher { glob: None, needle: pattern.to_lowercase() })
        }
    }

    /// None if no match; Some(true) when the file name itself matched a plain-text pattern.
    pub fn matches(&self, rel: &str) -> Option<bool> {
        match &self.glob {
            Some(glob) => glob.is_match(rel).then_some(false),
            None => {
                let lower = rel.to_lowercase();
                let name = lower.rsplit('/').next().unwrap_or(&lower);
                if name.contains(&self.needle) {
                    Some(true)
                } else {
                    lower.contains(&self.needle).then_some(false)
                }
            }
        }
    }
}

pub struct ReadFiles;

const DEFAULT_LINES: usize = 2000;

impl Tool for ReadFiles {
    fn name(&self) -> &'static str {
        "read_files"
    }
    fn title(&self) -> &'static str {
        "Read files"
    }
    fn description(&self) -> &'static str {
        "Read one or more text files (up to 20 per call) with line numbers as \"N: text\". Batch every file you need into one call. Each file returns up to 2000 lines by default; pass start_line/end_line (1-based, inclusive) to read a specific range, like sed -n '120,200p'. Long output is cut to stay small and says which start_line to continue from. Secret files (.env, keys) are never readable."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 20,
                    "description": "Files to read.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "e.g. \"myapp/src/server.ts\"" },
                            "start_line": { "type": "integer", "minimum": 1, "description": "First line to read (1-based). Default 1." },
                            "end_line": { "type": "integer", "minimum": 1, "description": "Last line to read, inclusive. Default start_line + 1999." }
                        },
                        "required": ["path"]
                    }
                }
            },
            "required": ["files"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let files: Vec<Value> = match args.get("files") {
            Some(Value::Array(items)) => items.clone(),
            // Be forgiving if a single path was passed at the top level.
            _ if arg_str(args, "path").is_some() => vec![args.clone()],
            _ => return Err(ToolError::new("Pass files: [{\"path\": \"folder/file.ext\"}].")),
        };
        if files.is_empty() || files.len() > 20 {
            return Err(ToolError::new("Pass between 1 and 20 files."));
        }
        let single = files.len() == 1;
        let mut out = Out::new();
        let mut skipped = Vec::new();
        for (i, spec) in files.iter().enumerate() {
            let path = match spec {
                Value::String(p) => p.as_str(),
                _ => arg_str(spec, "path").unwrap_or_default(),
            };
            if out.full {
                skipped.push(path.to_string());
                continue;
            }
            if i > 0 {
                out.line("");
            }
            let start = arg_usize(spec, "start_line", 1, usize::MAX).max(1);
            let end = spec.get("end_line").and_then(Value::as_u64).map(|n| n as usize);
            match read_one(ctx, path, start, end, &mut out) {
                Ok(()) => {}
                Err(e) if single => return Err(e),
                Err(e) => {
                    out.line(&format!("== {path} ==\nError: {}", e.0));
                }
            }
        }
        let note = (!skipped.is_empty()).then(|| {
            format!("[response limit reached; not read: {}. Request them in another call.]", skipped.join(", "))
        });
        Ok(out.finish(note))
    }
}

fn read_one(ctx: &Ctx, path: &str, start: usize, end: Option<usize>, out: &mut Out) -> Result<(), ToolError> {
    if path.is_empty() {
        return Err(ToolError::new("Each file needs a path."));
    }
    let r = ctx.ws.resolve(path)?;
    let shown = ctx.ws.display(&r);
    let text = read_text(&r.abs, &shown, 20 * 1024 * 1024)?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        out.line(&format!("== {shown} (empty file) =="));
        return Ok(());
    }
    if start > total {
        return Err(ToolError::new(format!("{shown} has only {total} lines; start_line {start} is past the end.")));
    }
    let end = end.unwrap_or(start + DEFAULT_LINES - 1).clamp(start, total);
    let header = if start == 1 && end == total {
        format!("== {shown} ({total} lines) ==")
    } else {
        format!("== {shown} (lines {start}-{end} of {total}) ==")
    };
    out.line(&header);
    for (i, line) in lines[start - 1..end].iter().enumerate() {
        let n = start + i;
        if !out.line(&format!("{n}: {}", clip_line(line))) {
            out.force(&format!("[cut at line {} to keep the response small; continue with start_line={n}]", n - 1));
            return Ok(());
        }
    }
    if end < total {
        out.line(&format!("[{} more lines; continue with start_line={}]", total - end, end + 1));
    } else if start == 1 {
        super::changes::mark_full(&r.abs);
    }
    Ok(())
}
