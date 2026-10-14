//! Everything that differs between macOS, Linux and Windows, behind one small API: paths,
//! randomness, the terminal (raw keys with a timeout, flushing typed-ahead keys, restoring it
//! when setup is killed), the clipboard and the browser. The background service lives in
//! service.rs. Supporting another OS means filling in this file and service.rs.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `fs::canonicalize` without Windows' `\\?\` prefix, so paths compare and display normally.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// Cryptographically secure random bytes from the OS.
pub fn random_bytes(buf: &mut [u8]) -> anyhow::Result<()> {
    getrandom::fill(buf).map_err(|e| anyhow::anyhow!("reading random bytes: {e}"))
}

/// Joins directories into a PATH value with the OS separator.
pub fn join_paths(parts: &[PathBuf]) -> String {
    std::env::join_paths(parts).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Folders that only hold cloud placeholders (iCloud "optimized storage", OneDrive files on
/// demand): reading into them would start downloads.
pub fn is_cloud_placeholder(meta: &std::fs::Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        meta.st_flags() & 0x4000_0000 != 0 // SF_DATALESS
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        meta.file_attributes() & (0x0004_0000 | 0x0040_0000) != 0
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = meta;
        false
    }
}

/// Lets the terminal understand ANSI escape codes (colors, cursor moves). Needed once at start on
/// Windows; a no-op elsewhere.
pub fn init_terminal() {
    #[cfg(windows)]
    win::enable_vt();
}

// ---- Installing and updating the binary ------------------------------------------------------

/// Puts a freshly downloaded binary in place of the running one. Windows can't overwrite a
/// running .exe but can rename it, so the old one moves aside and is deleted on the next start.
pub fn replace_running_exe(new: &Path, exe: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "aiva".into());
        let old = exe.with_file_name(format!("{stem}.old.exe"));
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old)?;
        if let Err(e) = std::fs::rename(new, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(e);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    std::fs::rename(new, exe)
}

/// Deletes the binary an update moved aside (Windows only; it can't be deleted while running).
pub fn remove_replaced_exe() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        for name in ["aiva.old.exe", "aivaw.old.exe"] {
            let _ = std::fs::remove_file(exe.with_file_name(name));
        }
    }
}

/// Removes the aiva folder, binary included. On Windows the running .exe can't be deleted, so
/// everything else goes now and a detached `cmd` removes the folder once this process exits.
pub fn remove_install_dir(dir: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = format!("ping -n 3 127.0.0.1 >NUL & rmdir /s /q \"{}\"", dir.display());
        std::process::Command::new("cmd.exe")
            .args(["/d", "/c"])
            .raw_arg(&script)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    std::fs::remove_dir_all(dir)
}

/// Takes `dir` out of the user's PATH (Windows; the installer added it there).
pub fn remove_from_user_path(dir: &Path) {
    #[cfg(windows)]
    {
        let script = format!(
            "$d='{}'; $p=[Environment]::GetEnvironmentVariable('Path','User'); if ($p) {{ [Environment]::SetEnvironmentVariable('Path', (($p -split ';') | Where-Object {{ $_ -and $_.TrimEnd('\\') -ne $d.TrimEnd('\\') }}) -join ';', 'User') }}",
            dir.display().to_string().replace('\'', "''")
        );
        let _ = std::process::Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", &script]).status();
    }
    #[cfg(not(windows))]
    let _ = dir;
}

// ---- Clipboard and browser -------------------------------------------------------------------

/// Copies text to the system clipboard. Returns false when no clipboard tool is available.
pub fn copy(text: &str) -> bool {
    use std::io::Write;
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        // Our text is ASCII (URLs and tokens), which clip.exe takes as-is.
        &[("clip.exe", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    };
    for (cmd, args) in tools {
        let mut command = std::process::Command::new(cmd);
        command.args(*args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null());
        if let Ok(mut child) = command.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return true;
            }
        }
    }
    false
}

/// Opens a URL in the default browser.
pub fn open_url(url: &str) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        // Not `cmd /c start`, which would interpret & and ^ in the URL.
        let mut c = std::process::Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    command.arg(url).status().is_ok_and(|s| s.success())
}

// ---- Terminal input ----------------------------------------------------------------------------

/// Discards keys typed before a prompt appeared (e.g. Enter pressed while setup was busy),
/// so they can't silently confirm the next prompt.
pub fn flush_input() {
    #[cfg(unix)]
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
    #[cfg(windows)]
    win::flush_input();
}

/// Keeps the terminal out of line mode (no echo, keys arrive at once, Ctrl+C arrives as a key)
/// so a screen can wait for a key with a timeout and animate meanwhile. Restores the previous
/// mode when dropped.
pub struct RawInput {
    #[cfg(unix)]
    saved: libc::termios,
    #[cfg(windows)]
    saved: u32,
}

impl RawInput {
    pub fn enable() -> Option<Self> {
        #[cfg(unix)]
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return None;
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(Self { saved })
        }
        #[cfg(windows)]
        {
            win::raw_mode().map(|saved| Self { saved })
        }
        #[cfg(not(any(unix, windows)))]
        None
    }

    /// True when a key is waiting to be read.
    pub fn wait(timeout: Duration) -> bool {
        #[cfg(unix)]
        unsafe {
            // select(), because macOS poll() can't wait on terminals.
            let mut set: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&mut set);
            libc::FD_SET(libc::STDIN_FILENO, &mut set);
            let mut tv = libc::timeval { tv_sec: timeout.as_secs() as _, tv_usec: timeout.subsec_micros() as _ };
            libc::select(libc::STDIN_FILENO + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv) > 0
        }
        #[cfg(windows)]
        {
            win::wait_key(timeout)
        }
        #[cfg(not(any(unix, windows)))]
        {
            std::thread::sleep(timeout);
            false
        }
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
        #[cfg(windows)]
        win::set_input_mode(self.saved);
    }
}

/// If the process is killed or the window closed while a full screen is up, write `reset` (leave
/// the alternate screen, show the cursor) and restore the terminal mode before dying.
pub fn guard_terminal(reset: &'static [u8]) {
    #[cfg(unix)]
    unix_guard::install(reset);
    #[cfg(windows)]
    win::guard(reset);
}

#[cfg(unix)]
mod unix_guard {
    static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
    static RESET: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();

    extern "C" fn restore(sig: libc::c_int) {
        unsafe {
            if let Some(t) = SAVED.get() {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t);
            }
            if let Some(reset) = RESET.get() {
                libc::write(libc::STDOUT_FILENO, reset.as_ptr() as *const _, reset.len());
            }
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    pub fn install(reset: &'static [u8]) {
        let _ = RESET.set(reset);
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
                let _ = SAVED.set(t);
            }
            for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
                libc::signal(sig, restore as *const () as libc::sighandler_t);
            }
        }
    }
}

#[cfg(windows)]
mod win {
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Console::*;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    fn input() -> Option<HANDLE> {
        let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode = 0;
        (h != INVALID_HANDLE_VALUE && !h.is_null() && unsafe { GetConsoleMode(h, &mut mode) } != 0).then_some(h)
    }

    pub fn enable_vt() {
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            unsafe {
                let h = GetStdHandle(which);
                let mut mode = 0;
                if GetConsoleMode(h, &mut mode) != 0 {
                    SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | ENABLE_PROCESSED_OUTPUT);
                }
            }
        }
        // UTF-8 for anything that goes through the console code page (box drawing, the mark).
        unsafe {
            SetConsoleOutputCP(65001);
        }
    }

    pub fn flush_input() {
        if let Some(h) = input() {
            unsafe {
                FlushConsoleInputBuffer(h);
            }
        }
    }

    /// Switches off line input, echo and Ctrl+C handling; returns the previous mode.
    pub fn raw_mode() -> Option<u32> {
        let h = input()?;
        let mut saved = 0;
        unsafe {
            GetConsoleMode(h, &mut saved);
            let raw = saved & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
            (SetConsoleMode(h, raw) != 0).then_some(saved)
        }
    }

    pub fn set_input_mode(mode: u32) {
        if let Some(h) = input() {
            unsafe {
                SetConsoleMode(h, mode);
            }
        }
    }

    /// Waits for a key press. The console input handle is also signaled by key releases, focus
    /// and mouse events; those are dropped here so the caller's read only ever sees a key press
    /// and never blocks.
    pub fn wait_key(timeout: Duration) -> bool {
        let Some(h) = input() else {
            std::thread::sleep(timeout);
            return false;
        };
        let started = Instant::now();
        loop {
            let left = timeout.saturating_sub(started.elapsed());
            if unsafe { WaitForSingleObject(h, left.as_millis() as u32) } != WAIT_OBJECT_0 {
                return false;
            }
            let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
            let mut count = 0;
            if unsafe { PeekConsoleInputW(h, &mut record, 1, &mut count) } == 0 || count == 0 {
                return false;
            }
            if record.EventType == KEY_EVENT as u16 && unsafe { record.Event.KeyEvent.bKeyDown } != 0 {
                return true;
            }
            unsafe {
                ReadConsoleInputW(h, &mut record, 1, &mut count);
            }
            if started.elapsed() >= timeout {
                return false;
            }
        }
    }

    static RESET: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
    static SAVED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

    unsafe extern "system" fn handler(_event: u32) -> windows_sys::core::BOOL {
        use std::io::Write;
        if let Some(mode) = SAVED.get() {
            set_input_mode(*mode);
        }
        if let Some(reset) = RESET.get() {
            let _ = std::io::stdout().write_all(reset);
            let _ = std::io::stdout().flush();
        }
        0 // let the default handler end the process
    }

    pub fn guard(reset: &'static [u8]) {
        let _ = RESET.set(reset);
        if let Some(h) = input() {
            let mut mode = 0;
            if unsafe { GetConsoleMode(h, &mut mode) } != 0 {
                let _ = SAVED.set(mode);
            }
        }
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
}
