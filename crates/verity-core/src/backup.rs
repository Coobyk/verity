//! `~/verity` backup into the group's General topic — config, session,
//! and the sqlite brain ride along as documents so a dead laptop (or a
//! move to a VM) doesn't lose the team. `verity restore` pulls it back.
//!
//! The upload carries `tg.session` (full account access) and
//! `state.sqlite` (every bot token) — it's only as private as the group.

use std::path::{Path, PathBuf};

use grammers_session::defs::PeerRef;

use crate::botapi::BotApi;
use crate::config::{chmod_private, Config};
use crate::mtproto::Tg;
use crate::store::Store;
use crate::{Error, Result};

/// Everything a backup ships.
pub const FILES: &[&str] = &["config.toml", "state.sqlite", "tg.session"];

/// Live sqlite dbs — a raw copy while `serve` writes can tear, so these
/// go through `VACUUM INTO` first.
const SQLITE_FILES: &[&str] = &["state.sqlite", "tg.session"];

const CAPTION: &str = "🗄 verity backup";
/// How far back a restore scans the channel for backup docs.
const RESTORE_SCAN: usize = 500;

/// Upload every `~/verity` file to the group's General topic (thread
/// `None`). Returns the file names sent.
pub async fn upload(api: &BotApi, group_id: i64) -> Result<Vec<String>> {
    let home = Config::home()?;
    let mut sent = Vec::new();
    for name in FILES {
        let path = home.join(name);
        if !path.exists() {
            continue;
        }
        let staged = if SQLITE_FILES.contains(name) {
            Some(snapshot(&path, name).await?)
        } else {
            None
        };
        api.send_document(
            group_id,
            staged.as_ref().unwrap_or(&path),
            Some(CAPTION),
            None,
        )
        .await?;
        sent.push((*name).to_string());
        if let Some(s) = &staged {
            let _ = s.parent().map(std::fs::remove_dir_all);
        }
    }
    Ok(sent)
}

/// `VACUUM INTO` a uniquely-named staging dir — keeps the uploaded file
/// name clean and stops two backups racing on one temp file. The brain
/// goes through [`Store`] (runs migrations); the MTProto session db is
/// grammers' schema, opened raw.
async fn snapshot(db: &Path, file_name: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "verity-backup-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)?;
    let snap = dir.join(file_name);
    let (db, snap2) = (db.to_path_buf(), snap.clone());
    let via_store = file_name == "state.sqlite";
    tokio::task::spawn_blocking(move || -> Result<()> {
        if via_store {
            Store::open(&db)?.vacuum_into(&snap2)
        } else {
            let conn = sqlite::Connection::open(&db)?;
            conn.execute("PRAGMA busy_timeout = 5000")?;
            let _ = std::fs::remove_file(&snap2);
            let esc = snap2.to_string_lossy().replace('\'', "''");
            conn.execute(format!("VACUUM INTO '{esc}'"))?;
            Ok(())
        }
    })
    .await
    .map_err(|e| Error::Other(e.to_string()))??;
    Ok(snap)
}

/// Pull the newest backup docs out of General back into `~/verity`,
/// overwriting what's there. Returns the file names restored.
pub async fn download(tg: &Tg, peer: PeerRef) -> Result<Vec<String>> {
    let docs = tg.find_general_documents(peer, FILES, RESTORE_SCAN).await?;
    let home = Config::home()?;
    std::fs::create_dir_all(&home)?;
    let mut got = Vec::new();
    for (name, msg) in docs {
        // Download beside the target, then rename into place — writing
        // `tg.session` directly would truncate the very session db this
        // client is running on mid-download. Stale -wal/-shm sidecars
        // from the replaced file must not survive either.
        let dest = home.join(&name);
        let tmp = home.join(format!("{name}.part"));
        msg.download_media(&tmp)
            .await
            .map_err(|e| Error::Mtproto(format!("download {name}: {e}")))?;
        std::fs::rename(&tmp, &dest)?;
        for ext in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(home.join(format!("{name}{ext}")));
        }
        chmod_private(&dest);
        got.push(name);
    }
    Ok(got)
}
