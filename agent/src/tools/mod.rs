//! The tools Claude can call. Each tool is a struct implementing [`Tool`] in its own module;
//! adding one means writing the struct and listing it in [`registry`]. The relay doesn't know
//! about individual tools: it advertises whatever [`definitions`] returns.
//!
//! Conventions shared by every tool (modeled on Claude Code, Codex, Gemini CLI and OpenCode):
//! - Paths are `<shared folder>/<relative path>`; output uses the same form.
//! - Line-numbered text is `N: text`, lines cut at [`MAX_LINE_CHARS`].
//! - Output stays under [`OUTPUT_BUDGET`] characters (about 8k tokens, below Claude's 10k warning)
//!   and always says how to get the rest when it is cut short.
//! - Failures are `isError` results that say what to do next.

pub mod changes;
mod edit;
mod matcher;
mod fs_tools;
mod git;
mod overview;
mod run;
mod search;
mod symbols;
mod workspace;

use crate::config::Config;
use serde_json::{Value, json};
pub use symbols::warm_up;
pub use workspace::Workspace;

/// Characters per tool result. Claude Code warns above ~10k tokens and cuts off at 25k.
pub const OUTPUT_BUDGET: usize = 32_000;
pub const MAX_LINE_CHARS: usize = 2_000;

pub struct Ctx<'a> {
    pub ws: &'a Workspace,
}

pub trait Tool: Sync {
    fn name(&self) -> &'static str;
    fn title(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    /// Read-only tools can run in parallel and never change anything.
    fn read_only(&self) -> bool {
        true
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError>;
}

fn registry() -> &'static [&'static dyn Tool] {
    &[
        &fs_tools::ListFolders,
        &overview::ProjectOverview,
        &fs_tools::ListDir,
        &fs_tools::FindFiles,
        &search::Search,
        &fs_tools::ReadFiles,
        &symbols::Outline,
        &symbols::FindSymbol,
        &search::FindReferences,
        &git::Git,
        &run::Run,
        &edit::EditFile,
        &edit::WriteFile,
        &edit::UndoChange,
    ]
}

/// The `tools/list` payload sent to the relay in `hello`. Tools that change files are only
/// offered while at least one shared folder allows edits.
pub fn definitions(cfg: &Config) -> Value {
    let writable = cfg.any_writable();
    Value::Array(
        registry()
            .iter()
            .filter(|t| t.read_only() || writable)
            .map(|t| {
                json!({
                    "name": t.name(),
                    "title": t.title(),
                    "description": t.description(),
                    "inputSchema": t.input_schema(),
                    "annotations": {
                        "title": t.title(),
                        "readOnlyHint": t.read_only(),
                        "destructiveHint": !t.read_only(),
                        "idempotentHint": t.read_only(),
                        "openWorldHint": false,
                    },
                })
            })
            .collect(),
    )
}

pub fn call(name: &str, args: &Value, cfg: &Config) -> ToolOutput {
    let Some(tool) = registry().iter().find(|t| t.name() == name) else {
        let names: Vec<_> = registry().iter().map(|t| t.name()).collect();
        return ToolError::new(format!("Unknown tool {name}. Available: {}.", names.join(", "))).into();
    };
    let ws = Workspace::new(cfg);
    let args = if args.is_null() { &json!({}) } else { args };
    match tool.run(args, &Ctx { ws: &ws }) {
        Ok(text) => ToolOutput { text, is_error: false },
        Err(e) => e.into(),
    }
}

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "content": [{ "type": "text", "text": self.text }] });
        if self.is_error {
            v["isError"] = Value::Bool(true);
        }
        v
    }
}

#[derive(Debug)]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(msg: impl Into<String>) -> ToolError {
        ToolError(msg.into())
    }

    /// Turns an OS error into advice, notably macOS privacy blocks on Documents/Desktop.
    pub fn io(e: &std::io::Error, what: &str) -> ToolError {
        match (e.kind(), e.raw_os_error()) {
            (_, Some(1)) if cfg!(target_os = "macos") => ToolError::new(format!(
                "macOS blocked access to {what}. On that Mac, open System Settings → Privacy & Security → Full Disk Access and allow aiva (or run `aiva doctor`)."
            )),
            (std::io::ErrorKind::PermissionDenied, _) => ToolError::new(format!("Permission denied reading {what}.")),
            (std::io::ErrorKind::NotFound, _) => ToolError::new(format!("Not found: {what}")),
            _ => ToolError::new(format!("Couldn't read {what}: {e}")),
        }
    }
}

impl From<ToolError> for ToolOutput {
    fn from(e: ToolError) -> ToolOutput {
        ToolOutput { text: e.0, is_error: true }
    }
}

// ---- Argument helpers -------------------------------------------------------------------

pub fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

pub fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    arg_str(args, key).ok_or_else(|| ToolError::new(format!("Missing required argument \"{key}\".")))
}

pub fn arg_usize(args: &Value, key: &str, default: usize, max: usize) -> usize {
    let v = args.get(key).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()));
    v.map(|n| n as usize).unwrap_or(default).clamp(0, max)
}

pub fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true"))).unwrap_or(default)
}

// ---- Output helpers ---------------------------------------------------------------------

/// Accumulates output up to [`OUTPUT_BUDGET`]; `line` returns false once the budget is spent.
pub struct Out {
    pub buf: String,
    limit: usize,
    pub full: bool,
}

impl Out {
    pub fn new() -> Out {
        Out::with_limit(OUTPUT_BUDGET)
    }

    pub fn with_limit(limit: usize) -> Out {
        Out { buf: String::new(), limit, full: false }
    }

    pub fn line(&mut self, s: &str) -> bool {
        if self.full || self.buf.len() + s.len() + 1 > self.limit {
            self.full = true;
            return false;
        }
        self.buf.push_str(s);
        self.buf.push('\n');
        true
    }

    /// Appends a short note even when the budget is spent (continuation hints).
    pub fn force(&mut self, s: &str) {
        self.buf.push_str(s);
        self.buf.push('\n');
        self.full = true;
    }

    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.buf.len())
    }

    pub fn finish(mut self, note: Option<String>) -> String {
        if let Some(note) = note {
            if !self.buf.is_empty() && !self.buf.ends_with("\n\n") {
                self.buf.push('\n');
            }
            self.buf.push_str(&note);
        }
        self.buf.trim_end().to_string()
    }
}

pub fn clip_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.len() <= MAX_LINE_CHARS {
        return line.into();
    }
    let mut end = MAX_LINE_CHARS;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [line cut at {MAX_LINE_CHARS} chars]", &line[..end]).into()
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / 1024.0 / 1024.0),
    }
}

/// Reads a file as UTF-8 text (lossy), refusing binaries and huge files.
pub fn read_text(path: &std::path::Path, shown: &str, max_bytes: u64) -> Result<String, ToolError> {
    let meta = std::fs::metadata(path).map_err(|e| ToolError::io(&e, shown))?;
    if meta.is_dir() {
        return Err(ToolError::new(format!("{shown} is a folder. Use list_dir to see what's inside.")));
    }
    if meta.len() > max_bytes {
        return Err(ToolError::new(format!(
            "{shown} is {} — too big to read whole. Read a line range, or use search to find the part you need.",
            human_size(meta.len())
        )));
    }
    let bytes = std::fs::read(path).map_err(|e| ToolError::io(&e, shown))?;
    changes::note_seen(path, &bytes, false);
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(ToolError::new(format!("{shown} is a binary file ({}), not text.", human_size(meta.len()))));
    }
    Ok(String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// Seconds-ago helper for "modified 3d ago" style hints.
pub fn ago(time: std::time::SystemTime) -> String {
    let secs = std::time::SystemTime::now().duration_since(time).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        s if s < 90 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 86_400 * 60 => format!("{}d ago", s / 86_400),
        s => format!("{}mo ago", s / (86_400 * 30)),
    }
}
