//! `run`: read-only command lines (rg, sed -n, awk, jq, wc, git log ... with pipes) inside a
//! shared folder. No shell is involved: the command is tokenized here, every program must be on
//! an allowlist, flags that write or execute are refused, and path arguments must stay inside
//! the shared folders and away from secret files.

use super::workspace::slash;
use super::{Ctx, Out, Tool, ToolError, arg_str, arg_usize, req_str};
use globset::GlobBuilder;
use regex::Regex;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const MAX_STAGES: usize = 8;
const MAX_CAPTURE: u64 = 4 * 1024 * 1024;

/// Programs that only read. Some get extra argument checks in `check_stage`.
const ALLOWED: &[&str] = &[
    "rg", "grep", "egrep", "fgrep", "sed", "awk", "jq", "cat", "head", "tail", "wc", "sort", "uniq", "cut", "tr", "nl",
    "paste", "comm", "join", "fold", "rev", "tac", "column", "diff", "cmp", "file", "stat", "ls", "tree", "du", "find",
    "basename", "dirname", "echo", "printf", "seq", "expand", "fmt", "od", "xxd", "hexdump", "strings", "shasum",
    "sha256sum", "md5", "md5sum", "git",
];

const GIT_READ: &[&str] = &[
    "log", "show", "diff", "status", "blame", "ls-files", "grep", "shortlog", "rev-parse", "rev-list",
    "describe", "branch", "tag", "reflog", "whatchanged", "name-rev", "merge-base", "count-objects",
];

/// Secret files that recursive grep/rg must skip (mirrors the workspace privacy rules).
const SECRET_GLOBS: &[&str] = &[
    ".env", ".env.*", "*.pem", "*.key", "*.p12", "*.pfx", "*.keystore", "*.jks", "*.kdbx", "*.tfstate", "id_rsa",
    "id_dsa", "id_ecdsa", "id_ed25519", ".npmrc", ".pypirc", ".netrc", ".git-credentials", ".pgpass",
];
const SECRET_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg"];

pub struct Run;

impl Tool for Run {
    fn name(&self) -> &'static str {
        "run"
    }
    fn title(&self) -> &'static str {
        "Run a read-only command"
    }
    fn description(&self) -> &'static str {
        "Run a read-only command line in a shared folder, e.g. \"rg -n 'useState' src | head -40\", \"sed -n '120,180p' src/app.ts\", \"jq '.scripts' package.json\", \"git log --oneline -20 -- src/api\", \"find . -name '*.sql' | wc -l\". Pipes (|), quotes and globs work; ; && > $() do not (no shell). Allowed: rg grep sed awk jq cat head tail wc sort uniq cut tr nl paste comm join diff file stat ls tree du find echo printf xxd strings shasum, and read-only git. Anything that writes or executes (sed -i, awk system(), find -exec, git commit...) is refused, as are secret files. Use it when the dedicated tools don't fit."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line, e.g. \"rg -n TODO src | wc -l\"." },
                "path": { "type": "string", "description": "Folder to run in (the working directory), e.g. \"myapp\" or \"myapp/src\". Required when several folders are shared." },
                "timeout_s": { "type": "integer", "minimum": 1, "maximum": 60, "description": "Default 20." }
            },
            "required": ["command"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let command = req_str(args, "command")?;
        let cwd = match arg_str(args, "path") {
            Some(p) => ctx.ws.resolve(p)?,
            None if ctx.ws.roots.len() == 1 => ctx.ws.all_roots().remove(0),
            None => {
                return Err(ToolError::new(format!("Pass path: the folder to run in ({}).", ctx.ws.folder_names())));
            }
        };
        if !cwd.abs.is_dir() {
            return Err(ToolError::new("path must be a folder (the working directory)."));
        }
        let timeout = Duration::from_secs(arg_usize(args, "timeout_s", 20, 60).max(1) as u64);

        let stages = parse(command)?;
        let mut prepared = Vec::with_capacity(stages.len());
        for stage in stages {
            let words = expand_globs(stage, &cwd.abs, ctx)?;
            prepared.push(check_stage(words, &cwd.abs, ctx)?);
        }
        execute(prepared, &cwd.abs, timeout)
    }
}

// ---- Tokenizing ---------------------------------------------------------------------------

/// A word and whether it came from unquoted text (only those get glob expansion).
struct Word {
    text: String,
    globbable: bool,
}

/// Splits a command line into pipeline stages of words, POSIX-quote style, refusing anything
/// that would need a real shell.
#[allow(unused_assignments)]
fn parse(line: &str) -> Result<Vec<Vec<Word>>, ToolError> {
    let mut stages: Vec<Vec<Word>> = vec![Vec::new()];
    let mut cur = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    let shell_only = |what: &str| {
        ToolError::new(format!(
            "{what} needs a shell, and run has none (it only supports pipes). Split it into separate run calls."
        ))
    };
    macro_rules! end_word {
        () => {
            if in_word {
                stages.last_mut().unwrap().push(Word { text: std::mem::take(&mut cur), globbable: !quoted });
                in_word = false;
                quoted = false;
            }
        };
    }
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                quoted = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err(ToolError::new("Unclosed single quote.")),
                    }
                }
            }
            '"' => {
                in_word = true;
                quoted = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\' | '$' | '`')) => cur.push(ch),
                            Some(ch) => {
                                cur.push('\\');
                                cur.push(ch);
                            }
                            None => return Err(ToolError::new("Unclosed double quote.")),
                        },
                        Some('$') if chars.peek().is_some_and(|n| *n == '(' || *n == '{' || n.is_alphabetic() || *n == '_') => {
                            return Err(shell_only("Variable or command substitution ($...)"));
                        }
                        Some('`') => return Err(shell_only("Command substitution (`...`)")),
                        Some(ch) => cur.push(ch),
                        None => return Err(ToolError::new("Unclosed double quote.")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(ch) = chars.next() {
                    cur.push(ch);
                }
            }
            ' ' | '\t' | '\n' => end_word!(),
            '|' => {
                if chars.peek() == Some(&'|') {
                    return Err(shell_only("||"));
                }
                end_word!();
                if stages.last().unwrap().is_empty() {
                    return Err(ToolError::new("Empty command before |."));
                }
                stages.push(Vec::new());
            }
            ';' => return Err(shell_only("Chaining with ;")),
            '&' => return Err(shell_only("& / &&")),
            '>' | '<' => return Err(shell_only("Redirection (> <)")),
            '`' => return Err(shell_only("Command substitution (`...`)")),
            '$' if chars.peek().is_some_and(|n| *n == '(' || *n == '{' || n.is_alphabetic() || *n == '_') => {
                return Err(shell_only("Variable or command substitution ($...)"));
            }
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    end_word!();
    if stages.iter().any(Vec::is_empty) {
        return Err(ToolError::new("Empty command."));
    }
    if stages.len() > MAX_STAGES {
        return Err(ToolError::new(format!("At most {MAX_STAGES} commands in one pipeline.")));
    }
    Ok(stages)
}

/// Expands unquoted words with * ? [ against files under the working directory, the way a
/// shell would (dotfiles only when the pattern asks for them; secret files never).
fn expand_globs(words: Vec<Word>, cwd: &Path, ctx: &Ctx) -> Result<Vec<String>, ToolError> {
    let mut out = Vec::new();
    for (i, w) in words.into_iter().enumerate() {
        let is_glob = w.globbable && i > 0 && !w.text.starts_with('-') && w.text.contains(['*', '?', '[']);
        if !is_glob {
            out.push(w.text);
            continue;
        }
        let pattern = w.text.trim_start_matches("./");
        let Ok(glob) = GlobBuilder::new(pattern).literal_separator(true).build() else {
            out.push(w.text);
            continue;
        };
        let matcher = glob.compile_matcher();
        let depth = if pattern.contains("**") { None } else { Some(pattern.matches('/').count() + 1) };
        let wants_dot = pattern.split('/').any(|seg| seg.starts_with('.'));
        let mut walker = ignore::WalkBuilder::new(cwd);
        walker.standard_filters(false).hidden(false).max_depth(depth).sort_by_file_name(|a, b| a.cmp(b));
        let mut hits = Vec::new();
        for entry in walker.build().flatten().filter(|e| e.depth() > 0) {
            let rel = slash(entry.path().strip_prefix(cwd).unwrap_or(entry.path()));
            if rel.split('/').any(|s| s == ".git" || s == "node_modules") && !pattern.contains("node_modules") {
                continue;
            }
            if !wants_dot && rel.split('/').any(|s| s.starts_with('.')) {
                continue;
            }
            if matcher.is_match(&rel) && !private(entry.path(), ctx) {
                hits.push(rel);
                if hits.len() > 5000 {
                    return Err(ToolError::new(format!("{} matches over 5000 files; narrow it, or use find/rg instead.", w.text)));
                }
            }
        }
        if hits.is_empty() {
            out.push(w.text); // like bash without nullglob
        } else {
            out.extend(hits);
        }
    }
    Ok(out)
}

// ---- Checking -----------------------------------------------------------------------------

fn private(abs: &Path, ctx: &Ctx) -> bool {
    ctx.ws.roots.iter().any(|r| {
        abs.strip_prefix(&r.path).is_ok_and(|rel| ctx.ws.is_private(&slash(rel), abs.is_dir()))
    })
}

fn lexical(cwd: &Path, arg: &str) -> PathBuf {
    let p = crate::config::expand_tilde(arg);
    let joined = if p.is_absolute() { p } else { cwd.join(p) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Any word that names an existing file or folder must be inside a shared folder and not secret.
fn check_paths(words: &[String], cwd: &Path, ctx: &Ctx) -> Result<(), ToolError> {
    for w in words.iter().skip(1) {
        // --flag=value: check the value too.
        let candidate = match w.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => value,
            _ if w.starts_with('-') => continue,
            _ => w.as_str(),
        };
        if candidate.is_empty() {
            continue;
        }
        let abs = lexical(cwd, candidate);
        let Ok(real) = crate::platform::canonicalize(&abs) else { continue }; // not a path (a pattern, a script...)
        let inside = ctx.ws.roots.iter().any(|r| real.starts_with(&r.path));
        if !inside {
            return Err(ToolError::new(format!("{candidate} is outside the shared folders.")));
        }
        if private(&real, ctx) {
            return Err(ToolError::new(format!("{candidate} is a secret file; aiva never reads those.")));
        }
    }
    Ok(())
}

static SED_DANGER: LazyLock<Regex> = LazyLock::new(|| {
    // w/W (write file) and e (execute) commands, or s///w / s///e flags.
    Regex::new(r"(?:^|[;{}\n]|[0-9$/!])\s*[wWe](?:\s|$)|/[gpIiMm0-9]*[we](?:\s|;|$)").unwrap()
});
static AWK_DANGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"system\s*\(|\|\s*getline|getline\s*<|print[f]?[^;{}]*[>|]|fflush|close\s*\("#).unwrap()
});

/// Whether a path (as written in a git `rev:path` argument) names a secret file.
fn is_secret_name(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let secret_dir = path.split('/').any(|seg| SECRET_DIRS.contains(&seg));
    let allowed_env = matches!(name, ".env.example" | ".env.sample" | ".env.template" | ".env.defaults");
    secret_dir
        || (!allowed_env
            && SECRET_GLOBS.iter().any(|g| globset::Glob::new(g).map(|g| g.compile_matcher().is_match(name)).unwrap_or(false)))
}

fn refuse(msg: impl Into<String>) -> ToolError {
    ToolError::new(format!("{} (run is read-only).", msg.into()))
}

/// Validates one pipeline stage and returns the final argv (with secret-file exclusions added
/// to recursive searches).
fn check_stage(mut words: Vec<String>, cwd: &Path, ctx: &Ctx) -> Result<Vec<String>, ToolError> {
    let prog = words[0].clone();
    if prog.contains('/') || !ALLOWED.contains(&prog.as_str()) {
        return Err(ToolError::new(format!(
            "{prog} isn't allowed. run only runs read-only programs: {}.",
            ALLOWED.join(" ")
        )));
    }
    let has = |pred: &dyn Fn(&str) -> bool| words.iter().skip(1).any(|w| pred(w));
    match prog.as_str() {
        "sed" => {
            if has(&|w| w.starts_with("-i") || w.starts_with("--in-place") || w == "-I" || (w.starts_with('-') && !w.starts_with("--") && w[1..].contains('i'))) {
                return Err(refuse("sed -i edits files"));
            }
            if words.iter().skip(1).any(|w| !w.starts_with('-') && SED_DANGER.is_match(w)) {
                return Err(refuse("sed scripts with w/W/e commands write files or run commands"));
            }
        }
        "awk" => {
            if words.iter().skip(1).any(|w| AWK_DANGER.is_match(w)) {
                return Err(refuse("awk with system(), getline from commands, or print to a file"));
            }
        }
        "find" => {
            if has(&|w| matches!(w, "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fprint" | "-fprint0" | "-fprintf" | "-fls")) {
                return Err(refuse("find -exec/-delete/-fprint"));
            }
            if has(&|w| matches!(w, "-L" | "-H" | "-follow")) {
                return Err(refuse("find -L/-follow could follow links out of the shared folder"));
            }
        }
        "rg" => {
            if has(&|w| w.starts_with("--pre") || w == "-z" || w == "--search-zip") {
                return Err(refuse("rg --pre / --search-zip run other programs"));
            }
            if has(&|w| w == "-L" || w == "--follow") {
                return Err(refuse("rg --follow could follow links out of the shared folder"));
            }
            check_paths(&words, cwd, ctx)?;
            for g in SECRET_GLOBS {
                words.push("-g".into());
                words.push(format!("!{g}"));
            }
            for d in SECRET_DIRS {
                words.push("-g".into());
                words.push(format!("!{d}/"));
            }
        }
        "grep" | "egrep" | "fgrep" => {
            if has(&|w| w == "-R" || w == "--dereference-recursive" || (w.starts_with('-') && !w.starts_with("--") && w.contains('R'))) {
                return Err(refuse("grep -R follows links out of the shared folder; use -r"));
            }
            check_paths(&words, cwd, ctx)?;
            for g in SECRET_GLOBS {
                words.push(format!("--exclude={g}"));
            }
            for d in SECRET_DIRS.iter().chain(&[".git", "node_modules"]) {
                words.push(format!("--exclude-dir={d}"));
            }
        }
        "sort" => {
            if has(&|w| w == "-o" || w.starts_with("--output") || (w.starts_with("-o") && w.len() > 2)) {
                return Err(refuse("sort -o writes a file"));
            }
        }
        "git" => {
            let sub = words.iter().skip(1).find(|w| !w.starts_with('-')).cloned().unwrap_or_default();
            check_paths(&words, cwd, ctx)?;
            if !GIT_READ.contains(&sub.as_str()) {
                return Err(ToolError::new(format!("git {sub} isn't allowed. Read-only git commands: {}.", GIT_READ.join(" "))));
            }
            if has(&|w| w.starts_with("--output") || w == "-c" || w.starts_with("--exec-path") || w.starts_with("--upload-pack") || w == "--ext-diff" || w.starts_with("--textconv") || w == "-O") {
                return Err(refuse("that git option writes files or runs programs"));
            }
            if matches!(sub.as_str(), "branch" | "tag") {
                let ok = ["-a", "-r", "-v", "-vv", "-l", "--list", "--all", "--remotes", "--show-current", "--merged", "--no-merged", "--contains", "--sort", "--points-at", "--format"];
                if words.iter().skip(2).any(|w| w.starts_with('-') && !ok.iter().any(|o| w == o || w.starts_with(&format!("{o}=")))) || words.iter().skip(2).filter(|w| !w.starts_with('-')).count() > 1 && !has(&|w| w == "-l" || w == "--list") {
                    return Err(refuse("git branch/tag can only list here"));
                }
            }
            if words.iter().skip(1).any(|w| w.split_once(':').is_some_and(|(_, p)| !p.is_empty() && is_secret_name(p))) {
                return Err(ToolError::new("That names a secret file; aiva never reads those, including from git history."));
            }
            if matches!(sub.as_str(), "show" | "diff" | "log" | "grep" | "whatchanged") {
                if !words.iter().any(|w| w == "--") {
                    words.push("--".into());
                    words.push(":(top)".into());
                }
                for g in SECRET_GLOBS {
                    words.push(format!(":(exclude,glob)**/{g}"));
                }
            }
            // Never let repo config run hooks, pagers or external tools.
            let mut argv = vec!["git".to_string()];
            for c in ["core.fsmonitor=false", "core.pager=cat", "color.ui=never", "diff.external=", if cfg!(windows) { "core.hooksPath=NUL" } else { "core.hooksPath=/dev/null" }] {
                argv.push("-c".into());
                argv.push(c.into());
            }
            argv.extend(words.drain(1..));
            words = argv;
        }
        _ => {}
    }
    if !matches!(prog.as_str(), "rg" | "grep" | "egrep" | "fgrep" | "git") {
        check_paths(&words, cwd, ctx)?;
    }
    Ok(words)
}

// ---- Running ------------------------------------------------------------------------------

/// PATH for commands, plus the usual install locations a background service doesn't inherit:
/// Homebrew, /usr/local, Cargo, and on Windows Git's bundled Unix tools (grep, sed, awk...).
fn search_path() -> String {
    let mut parts: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let home = dirs::home_dir().unwrap_or_default();
    let mut extras: Vec<PathBuf> = if cfg!(windows) {
        let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        vec![program_files.join(r"Git\cmd"), program_files.join(r"Git\usr\bin")]
    } else {
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from).to_vec()
    };
    extras.push(home.join(".cargo").join("bin"));
    for extra in extras {
        if !parts.contains(&extra) {
            parts.push(extra);
        }
    }
    crate::platform::join_paths(&parts)
}

fn execute(stages: Vec<Vec<String>>, cwd: &Path, timeout: Duration) -> Result<String, ToolError> {
    let path = search_path();
    let mut children: Vec<Child> = Vec::new();
    let mut stderr_readers = Vec::new();
    let mut prev_stdout: Option<std::process::ChildStdout> = None;
    let n = stages.len();
    for (i, argv) in stages.iter().enumerate() {
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(cwd)
            .env("PATH", &path)
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match prev_stdout.take() {
            Some(out) => {
                cmd.stdin(Stdio::from(out));
            }
            None => {
                cmd.stdin(Stdio::null());
            }
        }
        let mut child = cmd.spawn().map_err(|e| {
            for c in children.iter_mut() {
                let _ = c.kill();
            }
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError::new(format!("{} isn't installed on this computer.", argv[0]))
            } else {
                ToolError::new(format!("Couldn't start {}: {e}", argv[0]))
            }
        })?;
        let mut err = child.stderr.take().unwrap();
        stderr_readers.push(std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = (&mut err).take(64 * 1024).read_to_end(&mut s);
            String::from_utf8_lossy(&s).into_owned()
        }));
        if i + 1 < n {
            prev_stdout = child.stdout.take();
        }
        children.push(child);
    }
    let mut last_out = children.last_mut().unwrap().stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut last_out).take(MAX_CAPTURE).read_to_end(&mut buf);
        buf
    });

    let started = Instant::now();
    let mut timed_out = false;
    loop {
        if children.iter_mut().all(|c| c.try_wait().ok().flatten().is_some()) {
            break;
        }
        if started.elapsed() > timeout {
            timed_out = true;
            for c in children.iter_mut() {
                let _ = c.kill();
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let codes: Vec<Option<i32>> = children.iter_mut().map(|c| c.wait().ok().and_then(|s| s.code())).collect();
    let stdout = String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned();
    let stderr: Vec<String> = stderr_readers.into_iter().map(|h| h.join().unwrap_or_default()).collect();

    let mut out = Out::new();
    let total_lines = stdout.lines().count();
    let mut shown_lines = 0;
    for line in stdout.lines() {
        if !out.line(&super::clip_line(line)) {
            break;
        }
        shown_lines += 1;
    }
    if shown_lines < total_lines {
        out.force(&format!(
            "[output cut at {shown_lines} of {total_lines} lines; narrow it, e.g. add | head -50 or sed -n '{},{}p']",
            shown_lines + 1,
            shown_lines + 200
        ));
    }
    if stdout.is_empty() {
        out.force("(no output)");
    }
    let errs: Vec<&str> = stderr.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if !errs.is_empty() {
        out.force(&format!("stderr:\n{}", super::clip_line(&errs.join("\n")).chars().take(4000).collect::<String>()));
    }
    if timed_out {
        out.force(&format!("[stopped after {}s; narrow the command or raise timeout_s]", timeout.as_secs()));
    }
    let last = codes.last().copied().flatten();
    if last.is_some_and(|c| c != 0) {
        // grep/rg/diff use 1 for "no match"/"differs", which isn't a failure.
        out.force(&format!("[exit {}]", last.unwrap()));
    }
    Ok(out.finish(None))
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn words(line: &str) -> Vec<Vec<String>> {
        parse(line).unwrap().into_iter().map(|s| s.into_iter().map(|w| w.text).collect()).collect()
    }

    #[test]
    fn tokenizes_quotes_and_pipes() {
        assert_eq!(words(r#"rg -n 'a b' src | head -5"#), vec![vec!["rg", "-n", "a b", "src"], vec!["head", "-5"]]);
        assert_eq!(words(r#"awk '{print $1}' f"#), vec![vec!["awk", "{print $1}", "f"]]);
        assert_eq!(words(r#"echo "x\"y""#), vec![vec!["echo", "x\"y"]]);
    }

    #[test]
    fn refuses_shell_features() {
        for bad in ["ls; rm -rf /", "a && b", "a || b", "cat x > y", "echo $(id)", "echo `id`", "echo $HOME", "sleep 1 &"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
