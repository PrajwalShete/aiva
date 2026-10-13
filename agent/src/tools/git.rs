//! Read-only git history, so Claude can explain why code looks the way it does.

use super::{Ctx, Out, Tool, ToolError, arg_str, arg_usize};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Keep secrets out of diffs even when they are tracked in git.
const PRIVATE_PATHSPECS: &[&str] = &[
    ":(exclude,glob)**/.env",
    ":(exclude,glob)**/.env.*",
    ":(exclude,glob)**/*.pem",
    ":(exclude,glob)**/*.key",
    ":(exclude,glob)**/*.p12",
    ":(exclude,glob)**/*.pfx",
    ":(exclude,glob)**/id_rsa*",
    ":(exclude,glob)**/id_ed25519*",
    ":(exclude,glob)**/*.tfstate*",
];

pub struct Git;

impl Tool for Git {
    fn name(&self) -> &'static str {
        "git"
    }
    fn title(&self) -> &'static str {
        "Git history (read-only)"
    }
    fn description(&self) -> &'static str {
        "Read git information for a repository inside the shared folders: \"status\" (branch and uncommitted changes), \"log\" (recent commits, optionally for one file or folder), \"diff\" (uncommitted changes, or against a ref like \"main\" or \"HEAD~3\"), \"show\" (one commit's message and changes), \"blame\" (who last changed each line of a file). Use it to explain why code changed or what someone is working on. Never modifies the repository."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["status", "log", "diff", "show", "blame"] },
                "path": { "type": "string", "description": "Repository folder, or a file/subfolder to scope log/diff/show/blame to." },
                "ref": { "type": "string", "description": "Commit, branch or range: for show (default HEAD) and diff (default: uncommitted changes vs HEAD)." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Commits for log. Default 20." },
                "start_line": { "type": "integer", "minimum": 1, "description": "blame: first line. Default 1." },
                "end_line": { "type": "integer", "minimum": 1, "description": "blame: last line. Default start_line + 199." }
            },
            "required": ["action", "path"]
        })
    }
    fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let action = arg_str(args, "action").unwrap_or("status");
        let target = ctx.ws.resolve(arg_str(args, "path").unwrap_or_default())?;
        let is_file = target.abs.is_file();
        let dir = if is_file { target.abs.parent().unwrap_or(&target.abs).to_path_buf() } else { target.abs.clone() };
        let spec = target.abs.display().to_string();
        let git_ref = match arg_str(args, "ref") {
            Some(r) if valid_ref(r) => Some(r),
            Some(r) => return Err(ToolError::new(format!("\"{r}\" isn't a valid git ref."))),
            None => None,
        };

        let mut cmd: Vec<String> = match action {
            "status" => vec!["status".into(), "--short".into(), "--branch".into()],
            "log" => {
                let limit = arg_usize(args, "limit", 20, 200).max(1);
                vec!["log".into(), format!("-n{limit}"), "--date=relative".into(), "--pretty=format:%h  %ad  %an  %s".into(), "--".into(), spec.clone()]
            }
            "diff" => {
                let mut v = vec!["diff".into(), "--no-ext-diff".into(), "--no-textconv".into(), "--stat".into(), "--patch".into()];
                v.push(git_ref.unwrap_or("HEAD").into());
                v.push("--".into());
                v.push(spec.clone());
                v
            }
            "show" => {
                let mut v = vec!["show".into(), "--no-ext-diff".into(), "--no-textconv".into(), "--stat".into(), "--patch".into(), "--date=relative".into()];
                v.push(git_ref.unwrap_or("HEAD").into());
                v.push("--".into());
                v.push(spec.clone());
                v
            }
            "blame" => {
                if !is_file {
                    return Err(ToolError::new("blame needs a file path."));
                }
                let start = arg_usize(args, "start_line", 1, usize::MAX).max(1);
                let end = arg_usize(args, "end_line", start + 199, usize::MAX).max(start);
                vec!["blame".into(), "--date=short".into(), format!("-L{start},{end}"), "--".into(), spec.clone()]
            }
            other => return Err(ToolError::new(format!("Unknown action \"{other}\". Use status, log, diff, show or blame."))),
        };
        if matches!(action, "diff" | "show") {
            cmd.extend(PRIVATE_PATHSPECS.iter().map(|s| s.to_string()));
        }

        let output = run_git(&dir, &cmd)?;
        let shown = ctx.ws.display(&target);
        if output.trim().is_empty() {
            return Ok(match action {
                "diff" => format!("No uncommitted changes in {shown}."),
                "log" => format!("No commits touch {shown}."),
                _ => format!("Nothing to show for {shown}."),
            });
        }
        let mut out = Out::new();
        for line in output.lines() {
            if !out.line(&super::clip_line(line)) {
                out.force("[output cut to keep the response small; scope path to a single file or use a narrower ref]");
                break;
            }
        }
        Ok(out.finish(None))
    }
}

fn valid_ref(r: &str) -> bool {
    !r.starts_with('-') && r.len() <= 100 && r.chars().all(|c| c.is_ascii_alphanumeric() || "_./~^@{}-".contains(c))
}

/// Runs git with a timeout and settings that prevent hooks, pagers or external tools from running.
pub fn run_git(dir: &Path, args: &[String]) -> Result<String, ToolError> {
    let mut child = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.pager=cat", "-c", "color.ui=never", "-c", "core.quotepath=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::new(format!("Couldn't run git (is it installed?): {e}")))?;

    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.by_ref().take(4 * 1024 * 1024).read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| ToolError::new(e.to_string()))? {
            break status;
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            return Err(ToolError::new("git took longer than 20s; try a narrower path or ref."));
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    let stdout = reader.join().unwrap_or_default();
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        let err = err.trim();
        if err.contains("not a git repository") {
            return Err(ToolError::new("That folder isn't inside a git repository."));
        }
        return Err(ToolError::new(format!("git failed: {}", err.lines().next().unwrap_or("unknown error"))));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}
