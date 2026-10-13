//! project_overview: everything Claude needs to orient itself in one call.

use super::workspace::Resolved;
use super::{Ctx, Out, Tool, ToolError, arg_bool, arg_str, arg_usize};
use serde_json::{Value, json};
use std::collections::HashMap;

pub struct ProjectOverview;

impl Tool for ProjectOverview {
    fn name(&self) -> &'static str {
        "project_overview"
    }
    fn title(&self) -> &'static str {
        "Project overview"
    }
    fn description(&self) -> &'static str {
        "Start here. One call returns a project's languages, manifests (package.json, Cargo.toml, pyproject, go.mod...) with scripts and dependencies, likely entry points, git branch and recent activity, the start of the README, a folder tree, and a ranked map of the most-used classes and functions with line numbers. Omit path to get a short summary of every shared folder."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Project folder, e.g. \"myapp\" or \"monorepo/packages/api\". Omit to summarize every shared folder." },
                "tree_depth": { "type": "integer", "minimum": 0, "maximum": 5, "description": "Tree depth; 0 skips the tree. Default 2." },
                "map_size": { "type": "integer", "minimum": 0, "maximum": 20000, "description": "Characters for the symbol map; 0 skips it. Default 6000." },
                "include_readme": { "type": "boolean", "description": "Default true." }
            }
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let bases = ctx.ws.resolve_many(arg_str(args, "path"))?;
        let mut out = Out::new();
        if bases.len() > 1 {
            out.line(&format!("{} shared folders. Call project_overview with a path for the full picture.", bases.len()));
            for base in &bases {
                out.line("");
                summary(ctx, base, &mut out, true);
            }
            return Ok(out.finish(None));
        }
        let base = &bases[0];
        if base.abs.is_file() {
            return Err(ToolError::new("project_overview needs a folder. Use read_files or outline for a single file."));
        }
        summary(ctx, base, &mut out, false);

        if arg_bool(args, "include_readme", true) {
            readme(base, &mut out);
        }
        let depth = arg_usize(args, "tree_depth", 2, 5);
        if depth > 0 {
            out.line("");
            out.line(&format!("Tree (depth {depth}):"));
            let mut tree = Out::with_limit(out.remaining().min(8000));
            let (shown, seen) = super::fs_tools::tree(ctx, base, depth, 150, false, false, &mut tree);
            out.line(tree.buf.trim_end());
            if seen > shown {
                out.line(&format!("[{} more entries; use list_dir on a subfolder]", seen - shown));
            }
        }
        let map_size = arg_usize(args, "map_size", 6000, 20000).min(out.remaining().saturating_sub(200));
        if map_size > 500 {
            if let Some(map) = super::symbols::repo_map(ctx, base, map_size) {
                if !map.is_empty() {
                    out.line("");
                    out.line("Key definitions (most referenced first; \"line signature\"):");
                    out.line(map.trim_end());
                }
            }
        }
        out.line("");
        out.line("Next: outline or find_symbol to see code structure, search to locate text, read_files for the exact lines.");
        Ok(out.finish(None))
    }
}

fn summary(ctx: &Ctx, base: &Resolved, out: &mut Out, brief: bool) {
    let root = &ctx.ws.roots[base.root];
    let shown = ctx.ws.display(base);
    let real = if base.rel.is_empty() { crate::ui::tilde(&root.path) } else { crate::ui::tilde(&base.abs) };
    out.line(&format!("# {shown}  ({real})"));

    // Language mix by file extension.
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let mut files = 0;
    for entry in ctx.ws.walker(base, false).build().flatten().take(50_000) {
        if entry.file_type().is_some_and(|t| t.is_file()) {
            files += 1;
            if let Some(lang) = entry.path().extension().and_then(|e| e.to_str()).and_then(language) {
                *counts.entry(lang).or_default() += 1;
            }
        }
    }
    let mut langs: Vec<_> = counts.into_iter().collect();
    langs.sort_by(|a, b| b.1.cmp(&a.1));
    let langs: Vec<String> = langs.iter().take(8).map(|(l, n)| format!("{l} {n}")).collect();
    out.line(&format!("Files: {files}{}", if langs.is_empty() { String::new() } else { format!("  ·  {}", langs.join(", ")) }));

    for line in manifests(&base.abs) {
        out.line(&line);
    }
    if !brief {
        let entries = entry_points(ctx, base);
        if !entries.is_empty() {
            out.line(&format!("Likely entry points: {}", entries.join(", ")));
        }
    }
    if let Some(git) = git_summary(&base.abs) {
        out.line(&git);
    }
    if brief {
        let mut top: Vec<String> = std::fs::read_dir(&base.abs)
            .map(|d| {
                d.flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if name.starts_with('.') || ctx.ws.is_private(&name, false) {
                            return None;
                        }
                        Some(if e.path().is_dir() { format!("{name}/") } else { name })
                    })
                    .collect()
            })
            .unwrap_or_default();
        top.sort();
        let more = top.len().saturating_sub(25);
        top.truncate(25);
        out.line(&format!("Top level: {}{}", top.join("  "), if more > 0 { format!("  (+{more} more)") } else { String::new() }));
    }
}

fn language(ext: &str) -> Option<&'static str> {
    Some(match ext.to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => "TypeScript",
        "tsx" => "TSX",
        "js" | "mjs" | "cjs" => "JavaScript",
        "jsx" => "JSX",
        "py" | "pyi" => "Python",
        "rs" => "Rust",
        "go" => "Go",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "swift" => "Swift",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "C++",
        "cs" => "C#",
        "rb" => "Ruby",
        "php" => "PHP",
        "dart" => "Dart",
        "scala" => "Scala",
        "ex" | "exs" => "Elixir",
        "lua" => "Lua",
        "vue" => "Vue",
        "svelte" => "Svelte",
        "html" | "htm" => "HTML",
        "css" | "scss" | "sass" | "less" => "CSS",
        "md" | "mdx" => "Markdown",
        "json" | "jsonc" => "JSON",
        "yml" | "yaml" => "YAML",
        "toml" => "TOML",
        "sql" => "SQL",
        "sh" | "bash" | "zsh" => "Shell",
        "ino" => "Arduino",
        _ => return None,
    })
}

/// One line per recognized manifest at the project root, with the useful bits.
fn manifests(dir: &std::path::Path) -> Vec<String> {
    let mut lines = Vec::new();
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();

    if let Some(text) = read("package.json") {
        if let Ok(pkg) = serde_json::from_str::<Value>(&text) {
            let mut parts = vec![];
            if let Some(name) = pkg["name"].as_str() {
                parts.push(format!("\"{name}\"{}", pkg["version"].as_str().map(|v| format!(" v{v}")).unwrap_or_default()));
            }
            if let Some(scripts) = pkg["scripts"].as_object() {
                let names: Vec<_> = scripts.keys().take(12).cloned().collect();
                parts.push(format!("scripts: {}", names.join(", ")));
            }
            for (key, label) in [("dependencies", "deps"), ("devDependencies", "dev deps")] {
                if let Some(deps) = pkg[key].as_object() {
                    let names: Vec<_> = deps.keys().take(15).cloned().collect();
                    let more = deps.len().saturating_sub(15);
                    parts.push(format!("{label}: {}{}", names.join(", "), if more > 0 { format!(" (+{more})") } else { String::new() }));
                }
            }
            if let Some(ws) = pkg.get("workspaces") {
                parts.push(format!("workspaces: {}", ws));
            }
            lines.push(format!("package.json: {}", parts.join("; ")));
        }
    }
    if let Some(text) = read("Cargo.toml") {
        lines.push(format!("Cargo.toml: {}", toml_summary(&text, &["package", "workspace"], &["dependencies"])));
    }
    if let Some(text) = read("pyproject.toml") {
        lines.push(format!("pyproject.toml: {}", toml_summary(&text, &["project", "tool.poetry"], &["tool.poetry.dependencies"])));
    }
    if let Some(text) = read("requirements.txt") {
        let deps: Vec<_> = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).take(15).collect();
        lines.push(format!("requirements.txt: {}", deps.join(", ")));
    }
    if let Some(text) = read("go.mod") {
        let module = text.lines().find_map(|l| l.strip_prefix("module ")).unwrap_or("?");
        let requires = text.lines().filter(|l| l.starts_with('\t') || l.starts_with("require ")).count();
        lines.push(format!("go.mod: module {module}, ~{requires} requirements"));
    }
    let others: Vec<&str> = [
        "pom.xml", "build.gradle", "build.gradle.kts", "Gemfile", "composer.json", "pubspec.yaml", "Package.swift", "CMakeLists.txt",
        "Makefile", "Dockerfile", "docker-compose.yml", "compose.yaml", "wrangler.toml", "wrangler.jsonc", "vercel.json", "netlify.toml",
        "tsconfig.json", "vite.config.ts", "next.config.js", "next.config.mjs", "platformio.ini", "CLAUDE.md", "AGENTS.md",
    ]
    .into_iter()
    .filter(|n| dir.join(n).exists())
    .collect();
    if !others.is_empty() {
        lines.push(format!("Also: {}", others.join(", ")));
    }
    lines
}

/// Tiny TOML skim: name/version/description from the given tables plus dependency names.
fn toml_summary(text: &str, tables: &[&str], dep_tables: &[&str]) -> String {
    let mut section = String::new();
    let (mut name, mut version, mut deps) = (None, None, Vec::new());
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim().trim_matches('"'));
        if tables.contains(&section.as_str()) {
            match key {
                "name" if name.is_none() => name = Some(value.to_string()),
                "version" if version.is_none() => version = Some(value.to_string()),
                _ => {}
            }
        }
        if section == "dependencies" || dep_tables.contains(&section.as_str()) || section.ends_with(".dependencies") {
            deps.push(key.to_string());
        }
    }
    let mut parts = Vec::new();
    if let Some(n) = name {
        parts.push(format!("\"{n}\"{}", version.map(|v| format!(" v{v}")).unwrap_or_default()));
    }
    if !deps.is_empty() {
        let more = deps.len().saturating_sub(15);
        deps.truncate(15);
        parts.push(format!("deps: {}{}", deps.join(", "), if more > 0 { format!(" (+{more})") } else { String::new() }));
    }
    parts.join("; ")
}

fn entry_points(ctx: &Ctx, base: &Resolved) -> Vec<String> {
    const NAMES: &[&str] = &["main", "index", "app", "server", "cli", "lib", "__main__", "manage", "worker"];
    let mut found = Vec::new();
    let mut walker = ctx.ws.walker(base, false);
    walker.max_depth(Some(3));
    for entry in walker.build().flatten() {
        if found.len() >= 10 {
            break;
        }
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) || !super::symbols::supported(path) {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_lowercase();
        let rel = ctx.ws.rel_of(base, path);
        if NAMES.contains(&stem.as_str()) && !rel.contains("test") && !rel.contains("example") {
            found.push(ctx.ws.display_abs(base, path));
        }
    }
    found
}

fn git_summary(dir: &std::path::Path) -> Option<String> {
    let run = |args: &[&str]| super::git::run_git(dir, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).ok();
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let changed = run(&["status", "--porcelain"]).map(|s| s.lines().count()).unwrap_or(0);
    let last = run(&["log", "-1", "--date=relative", "--pretty=format:%h %ad: %s"]).unwrap_or_default();
    Some(format!(
        "Git: branch {}, {} · last commit {}",
        branch.trim(),
        if changed == 0 { "no uncommitted changes".to_string() } else { format!("{changed} changed file(s)") },
        if last.is_empty() { "none".into() } else { last }
    ))
}

fn readme(base: &Resolved, out: &mut Out) {
    let Ok(dir) = std::fs::read_dir(&base.abs) else { return };
    let Some(path) = dir.flatten().map(|e| e.path()).find(|p| {
        p.is_file() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_lowercase().starts_with("readme"))
    }) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("[![") && !l.trim_start().starts_with("<img") && !l.trim_start().starts_with("<p align"))
        .skip_while(|l| l.trim().is_empty())
        .take(25)
        .collect();
    if lines.is_empty() {
        return;
    }
    out.line("");
    out.line(&format!("{} (first {} lines):", path.file_name().unwrap().to_string_lossy(), lines.len()));
    for l in lines {
        out.line(&format!("  {}", super::clip_line(l)));
    }
}
