use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;

/// Everything needed to talk to the server after a successful login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub server: String,
    pub user_id: String,
    pub username: String,
    pub token: String,
}

/// Persisted access token.
///
/// Currently a mode 0600 file. The trait exists so a secret-service backend can
/// be added without touching callers.
pub trait TokenStore {
    fn load(&self) -> Result<Option<Credentials>>;
    fn store(&self, creds: &Credentials) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new() -> Result<Self> {
        Ok(Self {
            path: config::config_dir()?.join("credentials.toml"),
        })
    }
}

impl TokenStore for FileStore {
    fn load(&self) -> Result<Option<Credentials>> {
        if !self.path.exists() {
            return Ok(None);
        }

        // Refuse to read a token that other users can see rather than silently
        // continuing with a leaked secret.
        let mode = fs::metadata(&self.path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} is mode {:o}, refusing to use it; run: chmod 600 {}",
                self.path.display(),
                mode,
                self.path.display()
            );
        }

        let raw = fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        let creds = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", self.path.display()))?;
        Ok(Some(creds))
    }

    fn store(&self, creds: &Credentials) -> Result<()> {
        let dir = self
            .path
            .parent()
            .context("credentials path has no parent")?;
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

        // Mode is set at create time so the token is never briefly world-readable.
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        f.write_all(toml::to_string_pretty(creds)?.as_bytes())?;

        // An existing file keeps its old mode, so enforce it explicitly too.
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        if self.path.exists() {
            fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

/// Stable per-installation device id, generated once.
///
/// Jellyfin keys its session list on this, so a value that changed per start
/// would litter the server's dashboard with dead sessions.
pub fn device_id() -> Result<String> {
    let path = config::config_dir()?.join("device_id");
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    fs::create_dir_all(path.parent().context("no parent")?)?;
    fs::write(&path, &id)?;
    Ok(id)
}

/// Reads a password from the terminal without echoing it.
pub fn prompt_password(username: &str) -> Result<String> {
    let prompt = format!("Password for {username}: ");
    rpassword::prompt_password(prompt).context("reading password")
}
