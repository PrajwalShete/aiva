//! `~/.aiva/config.json`: which server to use, this computer's key, and the folders
//! Claude may look at. Re-read on every tool call, so `aiva folders add`
//! takes effect without restarting the service.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_SERVER: &str = "https://aiva.prajwalshete.com";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: String,
    /// This computer's secret key (`aiva_...`), generated locally by `aiva setup`. It is the
    /// token people paste when connecting an app; its hash is the connector URL's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default)]
    pub folders: Vec<Folder>,
    /// Extra privacy patterns (gitignore-style globs) on top of the built-in secret-file rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    /// The name Claude uses as the first segment of every path, e.g. `Jev/src/main.ts`.
    pub label: String,
    pub path: PathBuf,
    /// "read" (default) or "write" (Claude may also create and edit files here).
    #[serde(default = "default_access")]
    pub access: String,
}

fn default_access() -> String {
    "read".into()
}

impl Folder {
    pub fn writable(&self) -> bool {
        self.access == "write"
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: std::env::var("AIVA_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.into()),
            key: None,
            folders: Vec::new(),
            blocked: Vec::new(),
        }
    }
}

/// `$AIVA_HOME`, or `~/.aiva`.
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("AIVA_HOME") {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".aiva")
}

pub fn config_path() -> PathBuf {
    home().join("config.json")
}

pub fn log_path() -> PathBuf {
    home().join("logs").join("agent.log")
}

impl Config {
    pub fn load() -> Result<Option<Config>> {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn load_or_default() -> Result<Config> {
        Ok(Config::load()?.unwrap_or_default())
    }

    /// Loads the config, failing with a helpful message when setup hasn't run.
    pub fn require() -> Result<Config> {
        match Config::load()? {
            Some(cfg) if cfg.key.is_some() => Ok(cfg),
            _ => bail!("aiva isn't set up yet. Run `aiva setup` first."),
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = home();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = config_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// The public id in the connector URL: first 20 hex chars of SHA-256(key).
    pub fn id(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.key.as_deref().unwrap_or_default().as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..20].to_string()
    }

    pub fn connector_url(&self) -> String {
        format!("{}/{}/mcp", self.server.trim_end_matches('/'), self.id())
    }

    pub fn any_writable(&self) -> bool {
        self.folders.iter().any(Folder::writable)
    }

    /// Finds a shared folder by label or path.
    pub fn folder_mut(&mut self, target: &str) -> Option<&mut Folder> {
        let as_path = expand_tilde(target);
        let canonical = crate::platform::canonicalize(&as_path).unwrap_or(as_path);
        self.folders.iter_mut().find(|f| f.label.eq_ignore_ascii_case(target) || f.path == canonical)
    }

    /// Adds a folder, giving it a label that is unique among the shared folders.
    pub fn add_folder(&mut self, path: &Path) -> Result<&Folder> {
        let path = canonical_dir(path)?;
        if let Some(i) = self.folders.iter().position(|f| f.path == path) {
            return Ok(&self.folders[i]);
        }
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "root".into());
        let mut label = base.clone();
        let mut n = 2;
        while self.folders.iter().any(|f| f.label.eq_ignore_ascii_case(&label)) {
            label = format!("{base}-{n}");
            n += 1;
        }
        self.folders.push(Folder { label, path, access: default_access() });
        Ok(self.folders.last().unwrap())
    }

    /// Removes a folder by label or path. Returns the removed folder.
    pub fn remove_folder(&mut self, target: &str) -> Option<Folder> {
        let as_path = expand_tilde(target);
        let canonical = crate::platform::canonicalize(&as_path).unwrap_or(as_path);
        let i = self
            .folders
            .iter()
            .position(|f| f.label.eq_ignore_ascii_case(target) || f.path == canonical)?;
        Some(self.folders.remove(i))
    }
}

pub fn expand_tilde(input: &str) -> PathBuf {
    let input = input.trim();
    if input == "~" {
        return dirs::home_dir().unwrap_or_default();
    }
    if let Some(rest) = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\")) {
        return dirs::home_dir().unwrap_or_default().join(rest);
    }
    PathBuf::from(input)
}

/// Resolves a user-entered folder to an absolute, symlink-free directory path.
pub fn canonical_dir(path: &Path) -> Result<PathBuf> {
    let expanded = expand_tilde(&path.to_string_lossy());
    let abs = if expanded.is_absolute() { expanded } else { std::env::current_dir()?.join(expanded) };
    let canonical = crate::platform::canonicalize(&abs).with_context(|| format!("{} doesn't exist", abs.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a folder", canonical.display());
    }
    Ok(canonical)
}

/// A new random key: `aiva_` + 32 random bytes, base64url.
pub fn generate_key() -> Result<String> {
    let mut bytes = [0u8; 32];
    crate::platform::random_bytes(&mut bytes)?;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::from("aiva_");
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    Ok(out)
}
