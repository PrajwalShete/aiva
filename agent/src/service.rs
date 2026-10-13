//! Background service: a launchd LaunchAgent on macOS, a systemd user unit on Linux.
//! Both run `aiva run`, start at login, and restart the agent if it exits.

use crate::config;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub enum State {
    NotInstalled,
    Stopped,
    Running { pid: Option<u32> },
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::NotInstalled => write!(f, "not installed"),
            State::Stopped => write!(f, "installed, stopped"),
            State::Running { pid: Some(pid) } => write!(f, "running (pid {pid})"),
            State::Running { pid: None } => write!(f, "running"),
        }
    }
}

pub fn supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

/// The stable path the service should launch: the installed binary, not a temporary copy.
pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding the aiva binary")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn run(cmd: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(cmd).args(args).output().with_context(|| format!("running {cmd}"))
}

fn run_ok(cmd: &str, args: &[&str]) -> Result<()> {
    let out = run(cmd, args)?;
    if !out.status.success() {
        bail!("`{cmd} {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    const LABEL: &str = "com.aiva.agent";

    fn plist_path() -> PathBuf {
        dirs::home_dir().unwrap_or_default().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
    }

    fn domain() -> Result<String> {
        let out = run("id", &["-u"])?;
        Ok(format!("gui/{}", String::from_utf8_lossy(&out.stdout).trim()))
    }

    fn target() -> Result<String> {
        Ok(format!("{}/{LABEL}", domain()?))
    }

    fn xml(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub fn install(exe: &Path) -> Result<()> {
        let log = config::log_path();
        std::fs::create_dir_all(log.parent().unwrap())?;
        let env = match std::env::var("AIVA_HOME") {
            Ok(home) => format!(
                "  <key>EnvironmentVariables</key>\n  <dict><key>AIVA_HOME</key><string>{}</string></dict>\n",
                xml(&home)
            ),
            Err(_) => String::new(),
        };
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>Umask</key><integer>63</integer>
  <key>StandardInPath</key><string>/dev/null</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
{env}</dict>
</plist>
"#,
            exe = xml(&exe.display().to_string()),
            log = xml(&log.display().to_string()),
        );
        let path = plist_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, plist)?;
        let _ = run("launchctl", &["bootout", &target()?]);
        run_ok("launchctl", &["bootstrap", &domain()?, &path.display().to_string()])?;
        let _ = run("launchctl", &["enable", &target()?]);
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = run("launchctl", &["bootout", &target()?]);
        let path = plist_path();
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn start() -> Result<()> {
        if !plist_path().exists() {
            bail!("The service isn't installed. Run `aiva service install`.");
        }
        if matches!(state()?, State::Stopped) {
            run_ok("launchctl", &["bootstrap", &domain()?, &plist_path().display().to_string()])?;
        }
        Ok(())
    }

    pub fn stop() -> Result<()> {
        // bootout (rather than kill) so KeepAlive doesn't immediately restart it.
        let _ = run("launchctl", &["bootout", &target()?]);
        Ok(())
    }

    pub fn restart() -> Result<()> {
        match state()? {
            State::NotInstalled => bail!("The service isn't installed. Run `aiva service install`."),
            State::Stopped => start(),
            State::Running { .. } => run_ok("launchctl", &["kickstart", "-k", &target()?]),
        }
    }

    pub fn state() -> Result<State> {
        if !plist_path().exists() {
            return Ok(State::NotInstalled);
        }
        let out = run("launchctl", &["print", &target()?])?;
        if !out.status.success() {
            return Ok(State::Stopped);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        if !text.lines().any(|l| l.trim() == "state = running") {
            return Ok(State::Stopped);
        }
        let pid = text.lines().find_map(|l| l.trim().strip_prefix("pid = ")?.parse().ok());
        Ok(State::Running { pid })
    }

    pub fn describe() -> String {
        format!("launchd agent {LABEL} ({})", crate::ui::tilde(&plist_path()))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    const UNIT: &str = "aiva.service";

    fn unit_path() -> PathBuf {
        dirs::config_dir().unwrap_or_default().join("systemd/user").join(UNIT)
    }

    pub fn install(exe: &Path) -> Result<()> {
        let env = match std::env::var("AIVA_HOME") {
            Ok(home) => format!("Environment=AIVA_HOME={home}\n"),
            Err(_) => String::new(),
        };
        let unit = format!(
            "[Unit]\nDescription=aiva agent (lets Claude read your shared folders)\nWants=network-online.target\nAfter=network-online.target\nStartLimitBurst=10\nStartLimitIntervalSec=300\n\n[Service]\nExecStart=\"{}\" run\nRestart=always\nRestartSec=5\nUMask=0077\n{env}\n[Install]\nWantedBy=default.target\n",
            exe.display()
        );
        let path = unit_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, unit)?;
        run_ok("systemctl", &["--user", "daemon-reload"])?;
        run_ok("systemctl", &["--user", "enable", "--now", UNIT])?;
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = run("systemctl", &["--user", "disable", "--now", UNIT]);
        let path = unit_path();
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        Ok(())
    }

    pub fn start() -> Result<()> {
        run_ok("systemctl", &["--user", "start", UNIT])
    }

    pub fn stop() -> Result<()> {
        run_ok("systemctl", &["--user", "stop", UNIT])
    }

    pub fn restart() -> Result<()> {
        run_ok("systemctl", &["--user", "restart", UNIT])
    }

    pub fn state() -> Result<State> {
        if !unit_path().exists() {
            return Ok(State::NotInstalled);
        }
        let out = run("systemctl", &["--user", "show", UNIT, "--property=ActiveState,MainPID"])?;
        let text = String::from_utf8_lossy(&out.stdout);
        let active = text.lines().any(|l| l == "ActiveState=active");
        let pid = text.lines().find_map(|l| l.strip_prefix("MainPID=")?.parse().ok()).filter(|p| *p != 0);
        Ok(if active { State::Running { pid } } else { State::Stopped })
    }

    pub fn describe() -> String {
        format!("systemd user unit {UNIT} ({})", crate::ui::tilde(&unit_path()))
    }

    /// Without "linger", systemd stops user services when the user logs out.
    pub fn linger_enabled() -> Option<bool> {
        let user = std::env::var("USER").ok()?;
        let out = run("loginctl", &["show-user", &user, "--property=Linger"]).ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim() == "Linger=yes")
    }

    pub fn enable_linger() -> bool {
        let Ok(user) = std::env::var("USER") else { return false };
        run("loginctl", &["enable-linger", &user]).map(|o| o.status.success()).unwrap_or(false)
    }
}

#[cfg(not(target_os = "linux"))]
pub fn linger_enabled() -> Option<bool> {
    None
}

#[cfg(not(target_os = "linux"))]
pub fn enable_linger() -> bool {
    true
}

#[cfg(target_os = "linux")]
pub use imp::{enable_linger, linger_enabled};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::*;

    const UNSUPPORTED: &str =
        "Background service isn't supported on this OS yet. Keep `aiva run` open in a terminal instead.";

    pub fn install(_exe: &Path) -> Result<()> {
        bail!(UNSUPPORTED)
    }
    pub fn uninstall() -> Result<()> {
        Ok(())
    }
    pub fn start() -> Result<()> {
        bail!(UNSUPPORTED)
    }
    pub fn stop() -> Result<()> {
        bail!(UNSUPPORTED)
    }
    pub fn restart() -> Result<()> {
        bail!(UNSUPPORTED)
    }
    pub fn state() -> Result<State> {
        Ok(State::NotInstalled)
    }
    pub fn describe() -> String {
        "no background service on this OS".into()
    }
}

pub use imp::{describe, restart, start, state, stop, uninstall};

pub fn install(exe: &Path) -> Result<()> {
    imp::install(exe)
}

/// Restarts the service if it is running, so it picks up a new secret or binary.
pub fn restart_if_running() -> Result<bool> {
    if matches!(state()?, State::Running { .. }) {
        restart()?;
        return Ok(true);
    }
    Ok(false)
}
