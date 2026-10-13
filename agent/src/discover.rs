//! Finds code projects on this computer for the setup picker: folders with a .git directory or a
//! project manifest, in the usual places, newest first. Bounded in depth and time.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub struct Project {
    pub path: PathBuf,
    pub name: String,
    pub kind: String,
    pub modified: SystemTime,
}

const MARKERS: &[(&str, &str)] = &[
    ("Cargo.toml", "Rust"),
    ("go.mod", "Go"),
    ("pyproject.toml", "Python"),
    ("requirements.txt", "Python"),
    ("pom.xml", "Java"),
    ("build.gradle", "Java/Kotlin"),
    ("build.gradle.kts", "Kotlin"),
    ("Gemfile", "Ruby"),
    ("composer.json", "PHP"),
    ("pubspec.yaml", "Dart"),
    ("Package.swift", "Swift"),
    ("platformio.ini", "Arduino"),
    ("deno.json", "Deno"),
    ("package.json", "JavaScript"),
];

const SKIP: &[&str] = &[
    "node_modules", "Library", "Applications", "Movies", "Music", "Pictures", "Public", "target", "dist", "build", "vendor",
    "venv", "Pods", "DerivedData", "Downloads", "go", "snap",
];

fn kind_of(dir: &Path) -> Option<String> {
    let has = |f: &str| dir.join(f).exists();
    let mut kind = MARKERS.iter().find(|(f, _)| has(f)).map(|(_, k)| k.to_string());
    if kind.as_deref() == Some("JavaScript") && has("tsconfig.json") {
        kind = Some("TypeScript".into());
    }
    if kind.is_none() {
        let xcode = std::fs::read_dir(dir).ok()?.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".xcodeproj"));
        if xcode {
            kind = Some("Swift".into());
        }
    }
    let git = has(".git");
    match (kind, git) {
        (Some(k), true) => Some(format!("{k} · git")),
        (Some(k), false) => Some(k),
        (None, true) => Some("git".into()),
        (None, false) => None,
    }
}

fn modified(dir: &Path) -> SystemTime {
    [dir.to_path_buf(), dir.join(".git/index"), dir.join(".git/HEAD")]
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Project folders, most recently touched first (at most `limit`).
pub fn projects(limit: usize) -> Vec<Project> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut roots: Vec<(PathBuf, usize)> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        if cwd != home {
            roots.push((cwd, 2));
        }
    }
    for (name, depth) in [
        ("Documents", 3), ("Projects", 3), ("projects", 3), ("Code", 3), ("code", 3), ("dev", 3), ("Developer", 3),
        ("src", 3), ("workspace", 3), ("repos", 3), ("GitHub", 3), ("git", 3), ("Desktop", 2), ("Sites", 2),
    ] {
        let p = home.join(name);
        if p.is_dir() {
            roots.push((p, depth));
        }
    }
    roots.push((home.clone(), 2));

    let started = Instant::now();
    let budget = Duration::from_millis(1800);
    let mut found: Vec<Project> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (root, max_depth) in roots {
        let mut stack = vec![(root, 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if started.elapsed() > budget || found.len() > 400 {
                break;
            }
            let Ok(canonical) = std::fs::canonicalize(&dir) else { continue };
            if !seen.insert(canonical.clone()) {
                continue;
            }
            if depth > 0 {
                if let Some(kind) = kind_of(&canonical) {
                    let name = canonical.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    found.push(Project { modified: modified(&canonical), path: canonical, name, kind });
                    continue; // don't list folders inside a project
                }
            }
            if depth >= max_depth {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&canonical) else { continue };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                    continue;
                }
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push((e.path(), depth + 1));
                }
            }
        }
    }
    found.sort_by(|a, b| b.modified.cmp(&a.modified));
    found.truncate(limit);
    found
}
