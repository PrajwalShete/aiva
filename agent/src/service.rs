//! Background service: a launchd LaunchAgent on macOS, a systemd user unit on Linux, and on
//! Windows a per-user Run entry that starts `aivaw.exe` (see src/bin/aivaw.rs). All of them run
//! `aiva run`, start at login, and restart the agent if it exits. None needs admin rights.

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
    cfg!(any(target_os = "macos", target_os = "linux", windows))
}

/// The stable path the service should launch: the installed binary, not a temporary copy.
pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding the aiva binary")?;
    Ok(crate::platform::canonicalize(&exe).unwrap_or(exe))
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

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent};

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "aiva";
    /// Shared with src/bin/aivaw.rs.
    const MUTEX: &str = "Local\\aiva-agent";
    const STOP_EVENT: &str = "Local\\aiva-agent-stop";
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn launcher(exe: &Path) -> PathBuf {
        exe.with_file_name("aivaw.exe")
    }

    /// The Run entry's command line, if installed.
    fn entry() -> Option<String> {
        let out = run("reg.exe", &["query", RUN_KEY, "/v", VALUE]).ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().find(|l| l.trim_start().starts_with(VALUE))?;
        Some(line.split("REG_SZ").nth(1)?.trim().to_string())
    }

    fn running() -> bool {
        unsafe {
            let h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide(MUTEX).as_ptr());
            if h.is_null() {
                return false;
            }
            CloseHandle(h);
            true
        }
    }

    fn command_line(exe: &Path) -> String {
        let mut cmd = format!("\"{}\"", launcher(exe).display());
        if let Ok(home) = std::env::var("AIVA_HOME") {
            cmd.push_str(&format!(" --home \"{home}\""));
        }
        cmd
    }

    pub fn install(exe: &Path) -> Result<()> {
        if !launcher(exe).exists() {
            bail!("{} is missing. Reinstall aiva.", launcher(exe).display());
        }
        std::fs::create_dir_all(config::log_path().parent().unwrap())?;
        run_ok("reg.exe", &["add", RUN_KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &command_line(exe), "/f"])?;
        stop()?;
        start()
    }

    pub fn uninstall() -> Result<()> {
        stop()?;
        if entry().is_some() {
            run_ok("reg.exe", &["delete", RUN_KEY, "/v", VALUE, "/f"])?;
        }
        Ok(())
    }

    pub fn start() -> Result<()> {
        if entry().is_none() {
            bail!("The service isn't installed. Run `aiva service install`.");
        }
        if running() {
            return Ok(());
        }
        use std::os::windows::process::CommandExt;
        // Windows passes every inheritable handle to a child. Our own stdin/stdout/stderr
        // would then stay open in the long-running launcher, so whoever ran this command
        // (a script waiting for our output) would wait forever.
        unsafe {
            use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
            use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
            for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                SetHandleInformation(GetStdHandle(which), HANDLE_FLAG_INHERIT, 0);
            }
        }
        let exe = current_exe()?;
        let mut cmd = Command::new(launcher(&exe));
        if let Ok(home) = std::env::var("AIVA_HOME") {
            cmd.args(["--home", &home]);
        }
        cmd.creation_flags(DETACHED_PROCESS)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", launcher(&exe).display()))?;
        for _ in 0..50 {
            if running() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        bail!("The background launcher didn't start. See `aiva logs`.")
    }

    pub fn stop() -> Result<()> {
        unsafe {
            let h = OpenEventW(EVENT_MODIFY_STATE, 0, wide(STOP_EVENT).as_ptr());
            if h.is_null() {
                return Ok(());
            }
            SetEvent(h);
            CloseHandle(h);
        }
        for _ in 0..50 {
            if !running() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        bail!("The background launcher didn't stop.")
    }

    pub fn restart() -> Result<()> {
        if entry().is_none() {
            bail!("The service isn't installed. Run `aiva service install`.");
        }
        stop()?;
        start()
    }

    pub fn state() -> Result<State> {
        if entry().is_none() {
            return Ok(State::NotInstalled);
        }
        Ok(if running() { State::Running { pid: None } } else { State::Stopped })
    }

    pub fn describe() -> String {
        format!("startup entry {RUN_KEY}\\{VALUE} (runs aivaw.exe)")
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
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
