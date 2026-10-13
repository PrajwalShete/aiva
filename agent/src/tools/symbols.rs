//! Code structure via tree-sitter tags (the same queries GitHub code navigation and Aider use):
//! outline, find_symbol and the ranked symbol map in project_overview. Parsed files are cached
//! by (mtime, size) for the life of the agent, so repeat calls are near-instant.
//! Languages without a bundled grammar get a keyword-based fallback outline.

use super::workspace::Resolved;
use super::{Ctx, Out, Tool, ToolError, arg_bool, arg_str, arg_usize, clip_line, req_str};
use regex::Regex;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tree_sitter_tags::{TagsConfiguration, TagsContext};

const MAX_PARSE_BYTES: u64 = 1024 * 1024;

pub struct Def {
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    /// The first line of the definition, trimmed.
    pub sig: String,
    /// Nesting depth (0 = top level).
    pub depth: usize,
}

pub struct FileSymbols {
    pub defs: Vec<Def>,
    /// Distinct names this file references (calls, types...). Empty for fallback parsing.
    pub refs: HashSet<String>,
    pub precise: bool,
}

impl FileSymbols {
    /// The innermost definition spanning `line`.
    pub fn enclosing(&self, line: usize) -> Option<&Def> {
        self.defs
            .iter()
            .filter(|d| d.start_line <= line && line <= d.end_line && d.end_line > d.start_line)
            .min_by_key(|d| d.end_line - d.start_line)
    }

    fn parent_of(&self, def: &Def) -> Option<&Def> {
        self.defs
            .iter()
            .filter(|p| {
                p.start_line <= def.start_line
                    && def.end_line <= p.end_line
                    && (p.start_line, p.end_line) != (def.start_line, def.end_line)
            })
            .min_by_key(|p| p.end_line - p.start_line)
    }
}

struct Lang {
    exts: &'static [&'static str],
    build: fn() -> Option<TagsConfiguration>,
    config: OnceLock<Option<TagsConfiguration>>,
}

impl Lang {
    fn config(&self) -> Option<&TagsConfiguration> {
        self.config.get_or_init(self.build).as_ref()
    }
}

fn make(lang: tree_sitter::Language, query: &str) -> Option<TagsConfiguration> {
    TagsConfiguration::new(lang, query, "").ok()
}

fn typescript(lang: tree_sitter::Language) -> Option<TagsConfiguration> {
    // TypeScript's tags query builds on JavaScript's (as in the tree-sitter CLI's "inherits").
    let combined = format!("{}\n{}", tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY);
    make(lang.clone(), &combined).or_else(|| make(lang, tree_sitter_typescript::TAGS_QUERY))
}

/// Grammars are compiled on first use per language (a few ms to ~100ms each).
static LANGS: LazyLock<Vec<Lang>> = LazyLock::new(|| {
    let lang = |exts, build| Lang { exts, build, config: OnceLock::new() };
    vec![
        lang(&["rs"], || make(tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::TAGS_QUERY)),
        lang(&["py", "pyi"], || make(tree_sitter_python::LANGUAGE.into(), tree_sitter_python::TAGS_QUERY)),
        lang(&["js", "mjs", "cjs", "jsx"], || make(tree_sitter_javascript::LANGUAGE.into(), tree_sitter_javascript::TAGS_QUERY)),
        lang(&["ts", "mts", "cts"], || typescript(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())),
        lang(&["tsx"], || typescript(tree_sitter_typescript::LANGUAGE_TSX.into())),
        lang(&["go"], || make(tree_sitter_go::LANGUAGE.into(), tree_sitter_go::TAGS_QUERY)),
        lang(&["java"], || make(tree_sitter_java::LANGUAGE.into(), tree_sitter_java::TAGS_QUERY)),
    ]
});

/// Compiles every grammar up front (called in the background when the agent starts).
pub fn warm_up() {
    for lang in LANGS.iter() {
        let _ = lang.config();
    }
}

/// Source extensions handled by the keyword fallback.
const FALLBACK_EXTS: &[&str] = &[
    "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "cs", "kt", "kts", "swift", "rb", "php", "scala", "dart", "lua", "ex", "exs",
    "vue", "svelte", "sol", "zig", "m", "mm", "sh", "bash", "zsh",
];

static FALLBACK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:export\s+)?(?:default\s+)?(?:(?:pub(?:\([^)]*\))?|public|private|protected|internal|static|abstract|final|async|override|open|sealed|data|inline|suspend|virtual|extern|unsafe|const)\s+)*(class|struct|interface|enum|trait|impl|fn|func|function|def|defp|defmodule|module|namespace|object|protocol|extension|record|fun|contract)\s+([A-Za-z_][A-Za-z0-9_:]*)",
    )
    .unwrap()
});

thread_local! {
    static TAGS: RefCell<TagsContext> = RefCell::new(TagsContext::new());
}

type CacheEntry = (SystemTime, u64, Arc<FileSymbols>);
static CACHE: LazyLock<Mutex<HashMap<PathBuf, CacheEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn supported(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else { return false };
    let ext = ext.to_ascii_lowercase();
    LANGS.iter().any(|l| l.exts.contains(&ext.as_str())) || FALLBACK_EXTS.contains(&ext.as_str())
}

/// Symbols for one file, from cache when unchanged. None for unsupported or unreadable files.
pub fn file_symbols(path: &Path) -> Option<Arc<FileSymbols>> {
    if !supported(path) {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_PARSE_BYTES {
        return None;
    }
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if let Some((t, size, syms)) = CACHE.lock().unwrap().get(path) {
        if *t == mtime && *size == meta.len() {
            return Some(syms.clone());
        }
    }
    let source = std::fs::read(path).ok()?;
    if source[..source.len().min(8192)].contains(&0) {
        return None;
    }
    let syms = Arc::new(parse(path, &source));
    let mut cache = CACHE.lock().unwrap();
    if cache.len() > 50_000 {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), (mtime, meta.len(), syms.clone()));
    Some(syms)
}

fn parse(path: &Path, source: &[u8]) -> FileSymbols {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let line_starts: Vec<usize> = std::iter::once(0).chain(source.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1)).collect();
    let line_of = |byte: usize| line_starts.partition_point(|&s| s <= byte);
    let text_of_line = |line: usize| -> String {
        let start = line_starts[line - 1];
        let end = line_starts.get(line).copied().unwrap_or(source.len());
        let raw = String::from_utf8_lossy(&source[start..end]);
        let trimmed = raw.trim().trim_end_matches('{').trim_end();
        clip_line(trimmed).chars().take(140).collect()
    };

    if let Some(config) = LANGS.iter().find(|l| l.exts.contains(&ext.as_str())).and_then(Lang::config) {
        let parsed = TAGS.with(|ctx| {
            let mut ctx = ctx.borrow_mut();
            let Ok((tags, _)) = ctx.generate_tags(config, source, None) else { return None };
            let mut defs = Vec::new();
            let mut refs = HashSet::new();
            for tag in tags.flatten() {
                let name = String::from_utf8_lossy(&source[tag.name_range.clone()]).into_owned();
                if tag.is_definition {
                    let start_line = line_of(tag.range.start);
                    let end_line = line_of(tag.range.end.saturating_sub(1)).max(start_line);
                    defs.push(Def {
                        name,
                        kind: config.syntax_type_name(tag.syntax_type_id).to_string(),
                        start_line,
                        end_line,
                        sig: text_of_line(start_line),
                        depth: 0,
                    });
                } else {
                    refs.insert(name);
                }
            }
            Some((defs, refs))
        });
        if let Some((mut defs, refs)) = parsed {
            defs.sort_by_key(|d| (d.start_line, std::cmp::Reverse(d.end_line)));
            defs.dedup_by(|a, b| a.start_line == b.start_line && a.name == b.name);
            let spans: Vec<(usize, usize)> = defs.iter().map(|d| (d.start_line, d.end_line)).collect();
            for (i, d) in defs.iter_mut().enumerate() {
                d.depth = spans
                    .iter()
                    .enumerate()
                    .filter(|(j, (s, e))| *j != i && *s <= d.start_line && d.end_line <= *e && (*s, *e) != (d.start_line, d.end_line))
                    .count();
            }
            return FileSymbols { defs, refs, precise: true };
        }
    }

    // Keyword fallback: one line per definition, no end lines.
    let text = String::from_utf8_lossy(source);
    let defs = text
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let caps = FALLBACK_RE.captures(line)?;
            let indent = line.len() - line.trim_start().len();
            Some(Def {
                name: caps[2].to_string(),
                kind: caps[1].to_string(),
                start_line: i + 1,
                end_line: i + 1,
                sig: clip_line(line.trim().trim_end_matches('{').trim_end()).chars().take(140).collect(),
                depth: usize::from(indent > 0),
            })
        })
        .collect();
    FileSymbols { defs, refs: HashSet::new(), precise: false }
}

fn def_line(d: &Def, indent: usize) -> String {
    let range = if d.end_line > d.start_line { format!("{}-{}", d.start_line, d.end_line) } else { d.start_line.to_string() };
    format!("{}{range} {}", "  ".repeat(indent), d.sig)
}

/// Source files under `base` that have a parser, in path order.
fn source_files(ctx: &Ctx, base: &Resolved, limit: usize) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    for entry in ctx.ws.walker(base, false).build().flatten() {
        if files.len() >= limit {
            break;
        }
        if entry.file_type().is_some_and(|t| t.is_file()) && supported(entry.path()) {
            files.push((entry.path().to_path_buf(), ctx.ws.display_abs(base, entry.path())));
        }
    }
    files
}

/// Parses files in parallel (warming the cache) within a time budget.
fn parse_all(files: &[(PathBuf, String)], budget: Duration) -> Vec<Option<Arc<FileSymbols>>> {
    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<Arc<FileSymbols>>>> = files.iter().map(|_| Mutex::new(None)).collect();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(12);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= files.len() || started.elapsed() > budget {
                        break;
                    }
                    *results[i].lock().unwrap() = file_symbols(&files[i].0);
                }
            });
        }
    });
    results.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

pub struct Outline;

impl Tool for Outline {
    fn name(&self) -> &'static str {
        "outline"
    }
    fn title(&self) -> &'static str {
        "Outline functions and classes"
    }
    fn description(&self) -> &'static str {
        "List the classes, functions, methods, interfaces and types defined in a file, or in every source file under a folder, as \"start-end signature\" lines nested by scope. Much cheaper than reading whole files: use it to see how a file is organized, then read_files the line ranges you need. Precise for TS/JS, Python, Rust, Go and Java; keyword-based for other languages."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "A file or folder, e.g. \"myapp/src/server.ts\"." },
                "depth": { "type": "integer", "minimum": 0, "maximum": 5, "description": "Nesting levels to include (0 = top-level only). Default 1 for folders, 5 for a file." }
            },
            "required": ["path"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let base = ctx.ws.resolve(req_str(args, "path")?)?;
        let shown = ctx.ws.display(&base);
        let mut out = Out::new();
        if base.abs.is_file() {
            let depth = arg_usize(args, "depth", 5, 5);
            let Some(syms) = file_symbols(&base.abs) else {
                return Err(ToolError::new(format!("No outline available for {shown} (unsupported language or too large). Use read_files instead.")));
            };
            if syms.defs.is_empty() {
                return Ok(format!("{shown}: no definitions found."));
            }
            out.line(&format!("{shown}{}", if syms.precise { "" } else { " (keyword-based)" }));
            for d in syms.defs.iter().filter(|d| d.depth <= depth) {
                if !out.line(&def_line(d, d.depth + 1)) {
                    out.force("[outline cut short; ask for a narrower depth]");
                    break;
                }
            }
            return Ok(out.finish(None));
        }

        let depth = arg_usize(args, "depth", 1, 5);
        let files = source_files(ctx, &base, 3000);
        if files.is_empty() {
            return Ok(format!("No source files with outlines under {shown}."));
        }
        let parsed = parse_all(&files, Duration::from_secs(15));
        let mut done = 0;
        for ((_, display), syms) in files.iter().zip(&parsed) {
            let Some(syms) = syms else { continue };
            let defs: Vec<&Def> = syms.defs.iter().filter(|d| d.depth <= depth).collect();
            if defs.is_empty() {
                done += 1;
                continue;
            }
            let mut block = vec![display.clone()];
            block.extend(defs.iter().map(|d| def_line(d, d.depth + 1)));
            if block.iter().map(|l| l.len() + 1).sum::<usize>() > out.remaining() {
                break;
            }
            for line in &block {
                out.line(line);
            }
            done += 1;
        }
        let note = (done < files.len()).then(|| {
            format!("[outlined {done} of {} files; outline a subfolder or a single file for the rest]", files.len())
        });
        Ok(out.finish(note))
    }
}

pub struct FindSymbol;

impl Tool for FindSymbol {
    fn name(&self) -> &'static str {
        "find_symbol"
    }
    fn title(&self) -> &'static str {
        "Find a definition"
    }
    fn description(&self) -> &'static str {
        "Find where a function, class, method, type or interface is defined, by exact name (\"handleLogin\") or name path (\"AuthService/login\" for a method inside a class). With include_body=true it returns the full source of each match, which usually saves a separate read. Set substring=true to match partial names (\"login\" finds loginUser)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Symbol name or Parent/child name path." },
                "path": { "type": "string", "description": "Folder or file to look in. Omit for all shared folders." },
                "kind": { "type": "string", "description": "Only this kind: function, method, class, interface, module, type..." },
                "substring": { "type": "boolean", "description": "Match names containing the text, case-insensitively. Default false." },
                "include_body": { "type": "boolean", "description": "Include each definition's source code. Default false." },
                "max_results": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Default 20." }
            },
            "required": ["name"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let query = req_str(args, "name")?;
        let parts: Vec<&str> = query.split(['/', '.']).filter(|s| !s.is_empty()).collect();
        let name = *parts.last().unwrap_or(&query);
        let parent = (parts.len() > 1).then(|| parts[parts.len() - 2]);
        let substring = arg_bool(args, "substring", false);
        let kind = arg_str(args, "kind").map(str::to_lowercase);
        let body = arg_bool(args, "include_body", false);
        let max = arg_usize(args, "max_results", 20, 100).max(1);

        // Narrow to files that mention the name before parsing anything.
        let pattern = if substring { regex::escape(name) } else { format!(r"\b{}\b", regex::escape(name)) };
        let matcher = super::search::build_matcher(&pattern, false, Some(!substring))?;
        let opts = super::search::GrepOptions { context: 0, first_only: true, include_ignored: false, file_filter: None, file_type: None };
        let bases = ctx.ws.resolve_many(arg_str(args, "path"))?;
        let (hits, _) = super::search::grep(ctx, &bases, &matcher, &opts)?;

        let lower = name.to_lowercase();
        let mut found: Vec<(String, PathBuf, Arc<FileSymbols>, usize)> = Vec::new();
        for hit in &hits {
            let Some(syms) = file_symbols(&hit.abs) else { continue };
            for (i, d) in syms.defs.iter().enumerate() {
                let name_ok = if substring { d.name.to_lowercase().contains(&lower) } else { d.name == name };
                let kind_ok = kind.as_ref().is_none_or(|k| d.kind.to_lowercase().contains(k.as_str()));
                let parent_ok = parent.is_none_or(|p| {
                    syms.parent_of(d).is_some_and(|pd| pd.name == p) || has_word(&d.sig, p) || container_mentions(&hit.abs, d, p)
                });
                if name_ok && kind_ok && parent_ok {
                    found.push((hit.display.clone(), hit.abs.clone(), syms.clone(), i));
                }
            }
        }
        if found.is_empty() {
            return Ok(format!(
                "No definition named \"{query}\" found. Try substring=true, or search for the text if it's defined in an unsupported way."
            ));
        }
        // Prefer top-level definitions and shorter paths (likely the canonical one).
        found.sort_by_key(|(display, _, syms, i)| (syms.defs[*i].depth, display.len()));
        let total = found.len();
        let mut out = Out::new();
        out.line(&format!("{total} definition(s) of \"{query}\":"));
        for (display, abs, syms, i) in found.iter().take(max) {
            let d = &syms.defs[*i];
            let range = if d.end_line > d.start_line { format!("{}-{}", d.start_line, d.end_line) } else { d.start_line.to_string() };
            let parent = syms.parent_of(d).map(|p| format!("  (in {})", p.name)).unwrap_or_default();
            if !out.line(&format!("\n{display}:{range}  [{}]{parent}\n  {}", d.kind, d.sig)) {
                break;
            }
            if body {
                let text = super::read_text(abs, display, 20 * 1024 * 1024).unwrap_or_default();
                let end = d.end_line.min(d.start_line + 399);
                for (n, line) in text.lines().enumerate().skip(d.start_line - 1).take(end + 1 - d.start_line) {
                    if !out.line(&format!("{}: {}", n + 1, clip_line(line))) {
                        out.force(&format!("[body cut short; read_files {display} from start_line={}]", n + 1));
                        break;
                    }
                }
                if end < d.end_line && !out.full {
                    out.line(&format!("[body continues to line {}; read_files with start_line={}]", d.end_line, end + 1));
                }
            }
        }
        let note = (total > max).then(|| format!("[{} more; narrow with path or kind]", total - max));
        Ok(out.finish(note))
    }
}

/// Syntax errors in `source` for languages with a bundled grammar: up to 5 "line:col problem"
/// strings, empty when the file parses cleanly, None when the language isn't supported.
pub fn syntax_problems(path: &Path, source: &[u8]) -> Option<Vec<String>> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let lang = LANGS.iter().find(|l| l.exts.contains(&ext.as_str()))?.config()?.language.clone();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let mut problems = Vec::new();
    if !root.has_error() {
        return Some(problems);
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if problems.len() >= 5 {
            break;
        }
        if node.is_missing() {
            let p = node.start_position();
            problems.push(format!("{}:{} missing `{}`", p.row + 1, p.column + 1, node.kind()));
        } else if node.is_error() {
            let p = node.start_position();
            let snippet: String = String::from_utf8_lossy(&source[node.byte_range()]).chars().take(40).collect();
            problems.push(format!("{}:{} unexpected `{}`", p.row + 1, p.column + 1, snippet.split('\n').next().unwrap_or_default()));
        } else if node.has_error() {
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }
    problems.sort_by_key(|p| p.split(':').next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0));
    Some(problems)
}

fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == word)
}

/// For languages where methods sit in blocks that aren't tagged as definitions (Rust `impl X`,
/// some class bodies): checks whether the nearest less-indented line above mentions `parent`.
fn container_mentions(path: &Path, def: &Def, parent: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let lines: Vec<&str> = text.lines().collect();
    let Some(own) = lines.get(def.start_line.saturating_sub(1)) else { return false };
    let indent = |l: &str| l.len() - l.trim_start().len();
    let own_indent = indent(own);
    if own_indent == 0 {
        return false;
    }
    lines[..def.start_line - 1]
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty() && indent(l) < own_indent)
        .is_some_and(|l| has_word(l, parent))
}

/// Aider-style map of the most important definitions under `base`, ranked by how many other
/// files reference each name, fitted to `budget` characters.
pub fn repo_map(ctx: &Ctx, base: &Resolved, budget: usize) -> Option<String> {
    let files = source_files(ctx, base, 4000);
    if files.is_empty() {
        return None;
    }
    let parsed = parse_all(&files, Duration::from_secs(8));

    let mut referenced_by: HashMap<&str, HashSet<usize>> = HashMap::new();
    let mut defined_in: HashMap<&str, usize> = HashMap::new();
    for (fi, syms) in parsed.iter().enumerate() {
        let Some(syms) = syms else { continue };
        for r in &syms.refs {
            referenced_by.entry(r.as_str()).or_default().insert(fi);
        }
        let mut seen = HashSet::new();
        for d in &syms.defs {
            if seen.insert(d.name.as_str()) {
                *defined_in.entry(d.name.as_str()).or_default() += 1;
            }
        }
    }

    let mut scored: Vec<(f64, usize, usize)> = Vec::new(); // (score, file, def)
    for (fi, syms) in parsed.iter().enumerate() {
        let Some(syms) = syms else { continue };
        for (di, d) in syms.defs.iter().enumerate() {
            let refs = referenced_by.get(d.name.as_str()).map(|s| s.iter().filter(|f| **f != fi).count()).unwrap_or(0);
            let mut score = 1.0 + refs as f64;
            if d.name.starts_with('_') || d.name.len() < 3 {
                score *= 0.1;
            }
            if defined_in.get(d.name.as_str()).copied().unwrap_or(0) > 5 {
                score *= 0.1;
            }
            if matches!(d.kind.as_str(), "class" | "interface" | "struct" | "trait" | "module" | "type" | "enum") {
                score *= 1.5;
            }
            if d.depth > 0 {
                score *= 0.5;
            }
            scored.push((score, fi, di));
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));

    let mut chosen: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut size = 0;
    for (_, fi, di) in &scored {
        let d = &parsed[*fi].as_ref().unwrap().defs[*di];
        let cost = d.sig.len() + 10 + if chosen.contains_key(fi) { 0 } else { files[*fi].1.len() + 1 };
        if size + cost > budget {
            continue;
        }
        size += cost;
        chosen.entry(*fi).or_default().push(*di);
    }
    let mut file_order: Vec<usize> = chosen.keys().copied().collect();
    file_order.sort_by(|a, b| files[*a].1.cmp(&files[*b].1));
    let mut text = String::new();
    for fi in file_order {
        let syms = parsed[fi].as_ref().unwrap();
        let mut defs = chosen[&fi].clone();
        defs.sort_by_key(|di| syms.defs[*di].start_line);
        text.push_str(&files[fi].1);
        text.push('\n');
        for di in defs {
            let d = &syms.defs[di];
            text.push_str(&format!("  {} {}\n", d.start_line, d.sig));
        }
    }
    Some(text)
}
