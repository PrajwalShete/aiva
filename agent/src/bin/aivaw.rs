//! aivaw: the background launcher on Windows. It is a GUI-subsystem program, so starting it at
//! login (from the per-user Run key) opens no window. It runs `aiva.exe run` with a hidden
//! console, sends its output to the agent log, restarts it if it exits, and stops when
//! `aiva stop` signals it. One instance per user (a named mutex). macOS and Linux don't need it;
//! launchd and systemd do the same job there.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("aivaw is the Windows background launcher. Use `aiva run` or `aiva service install` here.");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    win::main();
}

#[cfg(windows)]
mod win {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, INFINITE, WaitForMultipleObjects, WaitForSingleObject};

    /// Shared with service.rs, which checks and stops the launcher through them.
    const MUTEX: &str = "Local\\aiva-agent";
    const STOP_EVENT: &str = "Local\\aiva-agent-stop";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn main() {
        // `--home <dir>` mirrors AIVA_HOME for test installs (the Run key can't set variables).
        let mut args = std::env::args().skip(1);
        let mut home: Option<PathBuf> = std::env::var_os("AIVA_HOME").map(PathBuf::from);
        while let Some(arg) = args.next() {
            if arg == "--home" {
                home = args.next().map(PathBuf::from);
            }
        }
        let home = home.unwrap_or_else(|| {
            std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".aiva")
        });
        let Ok(me) = std::env::current_exe() else { return };
        let agent = me.with_file_name("aiva.exe");
        let log_path = home.join("logs").join("agent.log");
        let _ = std::fs::create_dir_all(log_path.parent().unwrap());

        unsafe {
            // Hold the mutex for our lifetime; a second launcher just leaves.
            let _mutex = CreateMutexW(std::ptr::null(), 1, wide(MUTEX).as_ptr());
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return;
            }
            let stop = CreateEventW(std::ptr::null(), 1, 0, wide(STOP_EVENT).as_ptr());
            // If the launcher dies, the agent goes with it.
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );

            let mut backoff = Duration::from_secs(1);
            loop {
                let started = Instant::now();
                match spawn(&agent, &home, &log_path) {
                    Ok(mut child) => {
                        let handle = child.as_raw_handle() as HANDLE;
                        AssignProcessToJobObject(job, handle);
                        let handles = [handle, stop];
                        if WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) == WAIT_OBJECT_0 + 1 {
                            let _ = child.kill();
                            return;
                        }
                        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                        note(&log_path, &format!("aiva agent exited ({code}); restarting in {}s", backoff.as_secs()));
                    }
                    Err(e) => note(&log_path, &format!("Couldn't start {}: {e}", agent.display())),
                }
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                }
                if WaitForSingleObject(stop, backoff.as_millis() as u32) == WAIT_OBJECT_0 {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }

    fn spawn(agent: &std::path::Path, home: &std::path::Path, log_path: &std::path::Path) -> std::io::Result<std::process::Child> {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(log_path)?;
        std::process::Command::new(agent)
            .arg("run")
            .env("AIVA_HOME", home)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    }

    fn note(log_path: &std::path::Path, msg: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
            let _ = writeln!(f, "[aivaw] {msg}");
        }
    }
}
