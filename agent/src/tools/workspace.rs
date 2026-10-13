//! The sandbox every tool goes through: maps Claude-facing paths like `Jev/src/app.ts`
//! onto the shared folders, refuses anything outside them (including via symlinks or `..`),
//! and hides secret files.

use super::ToolError;
use crate::config::Config;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Component, Path, PathBuf};

/// Files that are never shown or read, whatever the user shares. Gitignore syntax.
const PRIVATE_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "!.env.example",
    "!.env.sample",
    "!.env.template",
    "!.env.defaults",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.keystore",
    "*.jks",
    "*.kdbx",
    "*.tfstate",
    "*.tfstate.backup",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".git-credentials",
    ".pgpass",
    "application_default_credentials.json",
    ".ssh/",
    ".aws/",
    ".gnupg/",
    ".docker/config.json",
];

/// Directories that are never walked (but can still be listed directly when asked for by path):
/// version control internals, dependencies and caches. Directories containing a CACHEDIR.TAG
/// (cargo's target/, many tool caches) are skipped too.
pub const SKIPPED_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", "node_modules", "__pycache__", ".venv", "venv", ".tox", ".mypy_cache", ".pytest_cache",
    ".ruff_cache", ".next", ".nuxt", ".svelte-kit", ".turbo", ".parcel-cache", ".cache", ".gradle", ".dart_tool",
    "DerivedData", "Pods", ".wrangler", ".terraform", ".pio",
];

pub struct Root {
    pub label: String,
    pub path: PathBuf,
    /// The user allowed edits in this folder (`aiva folders set <name> write`).
    pub writable: bool,
}

pub struct Workspace {
    pub roots: Vec<Root>,
    private: Gitignore,
}

/// A path inside one of the shared folders.
#[derive(Clone)]
pub struct Resolved {
    pub root: usize,
    pub abs: PathBuf,
    /// Relative to the root, with `/` separators; empty for the root itself.
    pub rel: String,
}

impl Workspace {
    pub fn new(cfg: &Config) -> Workspace {
        let roots = cfg
            .folders
            .iter()
            .map(|f| Root {
                label: f.label.clone(),
                path: std::fs::canonicalize(&f.path).unwrap_or_else(|_| f.path.clone()),
                writable: f.writable(),
            })
            .collect();
        let mut builder = GitignoreBuilder::new("");
        for pattern in PRIVATE_PATTERNS.iter().copied().chain(cfg.blocked.iter().map(String::as_str)) {
            let _ = builder.add_line(None, pattern);
        }
        let private = builder.build().unwrap_or_else(|_| Gitignore::empty());
        Workspace { roots, private }
    }

    pub fn display(&self, r: &Resolved) -> String {
        let label = &self.roots[r.root].label;
        if r.rel.is_empty() { label.clone() } else { format!("{label}/{}", r.rel) }
    }

    /// Display path for an absolute path found while walking `under`.
    pub fn display_abs(&self, under: &Resolved, abs: &Path) -> String {
        let root = &self.roots[under.root];
        match abs.strip_prefix(&root.path) {
            Ok(rel) if rel.as_os_str().is_empty() => root.label.clone(),
            Ok(rel) => format!("{}/{}", root.label, slash(rel)),
            Err(_) => abs.display().to_string(),
        }
    }

    pub fn rel_of(&self, under: &Resolved, abs: &Path) -> String {
        abs.strip_prefix(&self.roots[under.root].path).map(slash).unwrap_or_default()
    }

    pub fn is_private(&self, rel: &str, is_dir: bool) -> bool {
        !rel.is_empty() && self.private.matched_path_or_any_parents(rel, is_dir).is_ignore()
    }

    pub fn folder_names(&self) -> String {
        self.roots.iter().map(|r| r.label.as_str()).collect::<Vec<_>>().join(", ")
    }

    /// Every shared folder, for tools called without a path.
    pub fn all_roots(&self) -> Vec<Resolved> {
        (0..self.roots.len()).map(|root| Resolved { root, abs: self.roots[root].path.clone(), rel: String::new() }).collect()
    }

    /// Resolves an optional path: missing/empty/"." means every shared folder.
    pub fn resolve_many(&self, input: Option<&str>) -> Result<Vec<Resolved>, ToolError> {
        match input.map(str::trim) {
            None | Some("") | Some(".") | Some("/") => {
                if self.roots.is_empty() {
                    return Err(no_folders());
                }
                Ok(self.all_roots())
            }
            Some(p) => Ok(vec![self.resolve(p)?]),
        }
    }

    /// Finds which shared folder a Claude-facing path (`Label/rel/path`, or an absolute path
    /// inside a shared folder) belongs to, and its lexically normalized absolute path.
    fn locate(&self, input: &str) -> Result<(usize, PathBuf), ToolError> {
        if self.roots.is_empty() {
            return Err(no_folders());
        }
        let cleaned = input.trim().replace('\\', "/");
        let expanded = crate::config::expand_tilde(&cleaned);

        let (root, joined) = if expanded.is_absolute() {
            let root = self
                .roots
                .iter()
                .enumerate()
                .filter(|(_, r)| expanded.starts_with(&r.path))
                .max_by_key(|(_, r)| r.path.as_os_str().len())
                .map(|(i, _)| i)
                .ok_or_else(|| self.outside(input))?;
            (root, expanded)
        } else {
            let mut parts = cleaned.split('/').filter(|s| !s.is_empty() && *s != ".");
            let first = parts.next().unwrap_or_default();
            match self.roots.iter().position(|r| r.label == first || r.label.eq_ignore_ascii_case(first)) {
                Some(i) => (i, self.roots[i].path.join(parts.collect::<Vec<_>>().join("/"))),
                // With a single shared folder, accept paths relative to it.
                None if self.roots.len() == 1 => (0, self.roots[0].path.join(cleaned.trim_start_matches("./"))),
                None => {
                    return Err(ToolError::new(format!(
                        "Paths start with a shared folder name ({}). Example: \"{}/README.md\". Call list_folders to see them.",
                        self.folder_names(),
                        self.roots[0].label
                    )));
                }
            }
        };
        let abs = normalize(&joined);
        if !abs.starts_with(&self.roots[root].path) {
            return Err(self.outside(input));
        }
        Ok((root, abs))
    }

    /// Maps a Claude-facing path to an existing file or folder, refusing anything outside the
    /// shared folders (following symlinks) or marked private.
    pub fn resolve(&self, input: &str) -> Result<Resolved, ToolError> {
        let (root, abs) = self.locate(input)?;
        let root_path = &self.roots[root].path;
        // Follow symlinks and make sure the real target is still inside the shared folder.
        let real = match std::fs::canonicalize(&abs) {
            Ok(real) => real,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let rel = abs.strip_prefix(root_path).map(slash).unwrap_or_default();
                let shown = Resolved { root, abs: abs.clone(), rel };
                return Err(ToolError::new(format!("Not found: {}", self.display(&shown))));
            }
            Err(e) => return Err(ToolError::io(&e, input)),
        };
        if !real.starts_with(root_path) {
            return Err(ToolError::new(format!("{input} is a link to somewhere outside the shared folders.")));
        }
        let rel = real.strip_prefix(root_path).map(slash).unwrap_or_default();
        if self.is_private(&rel, real.is_dir()) {
            return Err(ToolError::new(format!(
                "{} is private: secret and key files are never shared.",
                self.display(&Resolved { root, abs: real.clone(), rel: rel.clone() })
            )));
        }
        Ok(Resolved { root, abs: real, rel })
    }

    /// Like [`resolve`](Self::resolve) but for writing: the file may not exist yet, the folder
    /// must allow edits, and version-control internals and secret files are off limits.
    /// For an existing symlink, returns the real target so the link itself survives.
    pub fn resolve_for_write(&self, input: &str) -> Result<Resolved, ToolError> {
        let (root, abs) = self.locate(input)?;
        let r = &self.roots[root];
        if !r.writable {
            return Err(ToolError::new(format!(
                "The folder \"{}\" is read-only. Ask the user to allow edits by running `aiva folders set {} write` on their computer.",
                r.label, r.label
            )));
        }
        // The deepest existing ancestor (or the file itself) must really be inside the folder.
        let mut probe = abs.as_path();
        let real = loop {
            match std::fs::canonicalize(probe) {
                Ok(real) => match abs.strip_prefix(probe) {
                    // Joining an empty path would add a trailing slash.
                    Ok(rest) if !rest.as_os_str().is_empty() => break real.join(rest),
                    _ => break real,
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => match probe.parent() {
                    Some(parent) => probe = parent,
                    None => return Err(self.outside(input)),
                },
                Err(e) => return Err(ToolError::io(&e, input)),
            }
        };
        if !real.starts_with(&r.path) || real == r.path {
            return Err(ToolError::new(format!("{input} isn't a file path inside a writable shared folder.")));
        }
        let rel = real.strip_prefix(&r.path).map(slash).unwrap_or_default();
        if rel.split('/').any(|seg| matches!(seg, ".git" | ".hg" | ".svn")) {
            return Err(ToolError::new("Version-control internals (.git) can't be edited."));
        }
        // Files that run code automatically or reconfigure AI tools: a prompt-injected edit there
        // would outlive the conversation.
        if rel.split('/').any(|seg| matches!(seg, ".husky" | ".githooks" | ".claude" | ".cursor"))
            || matches!(rel.rsplit('/').next(), Some(".envrc" | ".mcp.json"))
        {
            return Err(ToolError::new(format!(
                "{}/{rel} is a protected file (hooks and AI-tool settings run or change behavior automatically). The user can edit it directly.",
                r.label
            )));
        }
        if self.is_private(&rel, false) {
            return Err(ToolError::new(format!("{}/{rel} is a secret/key file; aiva never reads or writes those.", r.label)));
        }
        if real.is_dir() {
            return Err(ToolError::new(format!("{}/{rel} is a folder, not a file.", r.label)));
        }
        Ok(Resolved { root, abs: real, rel })
    }

    fn outside(&self, input: &str) -> ToolError {
        ToolError::new(format!("{input} is outside the shared folders ({}).", self.folder_names()))
    }

    /// A walker over `base` that respects .gitignore, skips heavy/VCS dirs and private files.
    pub fn walker(&self, base: &Resolved, include_ignored: bool) -> ignore::WalkBuilder {
        let mut builder = ignore::WalkBuilder::new(&base.abs);
        builder
            .hidden(false)
            .follow_links(false)
            .require_git(false)
            .git_ignore(!include_ignored)
            .git_exclude(!include_ignored)
            .ignore(!include_ignored)
            .parents(true)
            .git_global(false)
            .sort_by_file_name(|a, b| a.cmp(b));
        let root_path = self.roots[base.root].path.clone();
        let private = self.private.clone();
        builder.filter_entry(move |entry| {
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            if matches!(entry.file_name().to_str(), Some(".DS_Store" | "Thumbs.db" | "desktop.ini")) {
                return false;
            }
            if is_dir
                && entry.depth() > 0
                && (SKIPPED_DIRS.iter().any(|d| entry.file_name() == *d) || entry.path().join("CACHEDIR.TAG").exists())
            {
                return false;
            }
            let rel = entry.path().strip_prefix(&root_path).map(slash).unwrap_or_default();
            rel.is_empty() || !private.matched_path_or_any_parents(&rel, is_dir).is_ignore()
        });
        builder
    }
}

fn no_folders() -> ToolError {
    ToolError::new("No folders are shared yet. The user can add one with `aiva folders add <path>`.")
}

pub fn slash(p: &Path) -> String {
    let s = p.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' { s.into_owned() } else { s.replace(std::path::MAIN_SEPARATOR, "/") }
}

/// Lexically resolves `.` and `..` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
