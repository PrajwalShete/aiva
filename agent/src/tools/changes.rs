//! Every file change goes through here:
//! - atomic writes (temp file in the same folder, fsync, rename) that keep permissions,
//! - a journal with content-addressed backups in ~/.aiva/history so any change can be undone,
//! - read tracking, so a file the user changed in their editor isn't clobbered blind,
//! - one lock, so parallel tool calls can't interleave writes.

use super::ToolError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

static WRITE_LOCK: Mutex<()> = Mutex::new(());
/// Content hash of each file as Claude last saw it, and whether it saw all of it.
static SEEN: LazyLock<Mutex<HashMap<PathBuf, (String, bool)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const JOURNAL_KEEP: usize = 2000;

pub fn lock() -> MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Remember the content Claude has seen for `path` (read tools, and after writes). `full` is
/// true only when every line was shown.
pub fn note_seen(path: &Path, bytes: &[u8], full: bool) {
    let mut seen = SEEN.lock().unwrap();
    if seen.len() > 20_000 {
        seen.clear();
    }
    seen.insert(path.to_path_buf(), (hash(bytes), full));
}

/// Upgrades the last read of `path` to a full read (read_files showed every line).
pub fn mark_full(path: &Path) {
    if let Some(entry) = SEEN.lock().unwrap().get_mut(path) {
        entry.1 = true;
    }
}

pub fn seen(path: &Path) -> Option<(String, bool)> {
    SEEN.lock().unwrap().get(path).cloned()
}

/// Writes a file: atomically for normal files; in place for hard-linked files so the other
/// links keep seeing the content (the backup is already durable in the journal by then).
pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(path).is_ok_and(|m| m.nlink() > 1) {
            let mut f = std::fs::OpenOptions::new().write(true).truncate(true).open(path)?;
            f.write_all(bytes)?;
            return f.sync_all();
        }
    }
    atomic_write(path, bytes)
}

/// Replaces `path` atomically, keeping the existing file's permissions (new files get 0644
/// minus umask via File::create). Parent folders are created as needed.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no parent folder"))?;
    std::fs::create_dir_all(dir)?;
    let perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.aiva-{}.tmp", std::process::id() as u64 * 1_000_003 + nanos()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Some(perms) = perms {
            std::fs::set_permissions(&tmp, perms)?;
        }
        std::fs::rename(&tmp, path)?;
        // Make the rename itself durable.
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn nanos() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0)
}

// ---- Journal ------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub display: String,
    /// Backup blob of the content before the change; None = the file didn't exist.
    pub before: Option<String>,
    /// Hash of the content after the change; None = the file was removed.
    pub after: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Change {
    pub id: u64,
    pub time: u64,
    pub tool: String,
    pub summary: String,
    pub files: Vec<FileChange>,
    #[serde(default)]
    pub undone: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Line {
    Change(Change),
    Undo { undo: u64 },
}

fn history_dir() -> PathBuf {
    crate::config::home().join("history")
}

fn blob_path(hash: &str) -> PathBuf {
    history_dir().join("blobs").join(&hash[..2]).join(hash)
}

fn store_blob(bytes: &[u8]) -> std::io::Result<String> {
    let h = hash(bytes);
    let path = blob_path(&h);
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap())?;
        atomic_write(&path, bytes)?;
    }
    Ok(h)
}

/// A pending file change for [`record`]: (real path, display path, before bytes, after bytes).
pub type Pending<'a> = (PathBuf, String, Option<&'a [u8]>, Option<&'a [u8]>);

/// Appends a change to the journal, backing up the old contents first. Call BEFORE writing the
/// file so a backup always exists; call [`abort`] if the write then fails.
pub fn record(tool: &str, summary: &str, files: Vec<Pending>) -> std::io::Result<u64> {
    std::fs::create_dir_all(history_dir())?;
    let mut entries = Vec::with_capacity(files.len());
    for (path, display, before, after) in files {
        entries.push(FileChange {
            path,
            display,
            before: before.map(store_blob).transpose()?,
            after: after.map(hash),
        });
    }
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let id = load().last().map(|c| c.id + 1).unwrap_or(1);
    let change = Change { id, time, tool: tool.into(), summary: summary.into(), files: entries, undone: false };
    append(&Line::Change(change))?;
    Ok(id)
}

/// Marks a recorded change as not applied (its write failed).
pub fn abort(id: u64) {
    let _ = append(&Line::Undo { undo: id });
}

fn append(line: &Line) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(history_dir().join("journal.jsonl"))?;
    writeln!(f, "{}", serde_json::to_string(line)?)?;
    f.sync_data()
}

/// All changes, oldest first, with undo markers applied.
pub fn load() -> Vec<Change> {
    let Ok(f) = std::fs::File::open(history_dir().join("journal.jsonl")) else { return Vec::new() };
    let mut changes: Vec<Change> = Vec::new();
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        match serde_json::from_str::<Line>(&line) {
            Ok(Line::Change(c)) => changes.push(c),
            Ok(Line::Undo { undo }) => {
                if let Some(c) = changes.iter_mut().find(|c| c.id == undo) {
                    c.undone = true;
                }
            }
            Err(_) => {}
        }
    }
    let skip = changes.len().saturating_sub(JOURNAL_KEEP);
    changes.drain(..skip);
    changes
}

/// Reverts one change: the given id, or the latest not-yet-undone change (optionally the
/// latest touching `only`). Refuses if a file was modified after the change.
pub fn undo(id: Option<u64>, only: Option<&Path>) -> Result<Change, ToolError> {
    let _guard = lock();
    let changes = load();
    let change = match id {
        Some(id) => changes.iter().find(|c| c.id == id).ok_or_else(|| ToolError::new(format!("No change #{id} in the history.")))?,
        None => changes
            .iter()
            .rev()
            .find(|c| !c.undone && only.is_none_or(|p| c.files.iter().any(|f| f.path == p)))
            .ok_or_else(|| ToolError::new("There's nothing left to undo."))?,
    };
    if change.undone {
        return Err(ToolError::new(format!("Change #{} was already undone.", change.id)));
    }
    // Check every file first so an undo is all-or-nothing.
    for f in &change.files {
        let current = std::fs::read(&f.path).ok();
        if current.as_deref().map(hash) != f.after {
            return Err(ToolError::new(format!(
                "{} was modified after change #{}, so undoing it would lose those newer edits. Undo the later changes first, or edit the file directly.",
                f.display, change.id
            )));
        }
    }
    for f in &change.files {
        match &f.before {
            Some(blob) => {
                let bytes = std::fs::read(blob_path(blob)).map_err(|e| ToolError::new(format!("Backup for {} is missing: {e}", f.display)))?;
                write(&f.path, &bytes).map_err(|e| ToolError::io(&e, &f.display))?;
                note_seen(&f.path, &bytes, true);
            }
            None => {
                std::fs::remove_file(&f.path).map_err(|e| ToolError::io(&e, &f.display))?;
            }
        }
    }
    append(&Line::Undo { undo: change.id }).map_err(|e| ToolError::new(format!("Undone, but the journal couldn't be updated: {e}")))?;
    Ok(change.clone())
}
