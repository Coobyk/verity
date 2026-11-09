//! `~/.verity` layout: `config.toml`, `state.sqlite`, `tg.session`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Best-effort 0600 on a file holding credentials (config, sqlite with
/// bot tokens, MTProto session). No-op off unix.
pub(crate) fn chmod_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// MTProto app credentials from https://my.telegram.org
    pub api_id: Option<i32>,
    pub api_hash: Option<String>,
    /// The forum supergroup that hosts the bots ("Verity HQ").
    pub group_id: Option<i64>,
    /// Binary used by the Pi harness (`pi` or `pirs`).
    pub pi_bin: Option<String>,
}

impl Config {
    pub fn home() -> Result<PathBuf> {
        if let Ok(h) = std::env::var("VERITY_HOME") {
            return Ok(PathBuf::from(h));
        }
        dirs::home_dir()
            .map(|h| h.join(".verity"))
            .ok_or_else(|| Error::Config("cannot resolve home directory".into()))
    }

    fn path() -> Result<PathBuf> {
        Ok(Self::home()?.join("config.toml"))
    }

    pub fn session_path() -> Result<PathBuf> {
        Ok(Self::home()?.join("tg.session"))
    }

    pub fn db_path() -> Result<PathBuf> {
        Ok(Self::home()?.join("state.sqlite"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(&path)?;
        toml::from_str(&s).map_err(|e| Error::Config(e.to_string()))
    }

    pub fn save(&self) -> Result<()> {
        let home = Self::home()?;
        std::fs::create_dir_all(&home)?;
        let s = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        // credentials-adjacent: keep config owner-readable only
        let path = Self::path()?;
        std::fs::write(&path, s)?;
        chmod_private(&path);
        Ok(())
    }

    pub fn api_credentials(&self) -> Result<(i32, String)> {
        let id = self
            .api_id
            .or_else(|| std::env::var("TG_API_ID").ok().and_then(|v| v.parse().ok()));
        let hash = self
            .api_hash
            .clone()
            .or_else(|| std::env::var("TG_API_HASH").ok());
        match (id, hash) {
            (Some(id), Some(hash)) => Ok((id, hash)),
            _ => Err(Error::Config(
                "missing api_id/api_hash — set them via `verity config` or TG_API_ID/TG_API_HASH"
                    .into(),
            )),
        }
    }
}
