//! `~/verity` backup into the group's General topic — one 7z archive
//! carrying the whole install (minus `target/`) plus a `_home/` overlay
//! for settings that live outside it (systemd units, per-bot send
//! state). `verity restore` pulls the newest archive back and unpacks.
//!
//! The upload carries `tg.session` (full account access) and
//! `state.sqlite` (every bot token) — it's only as private as the group.

use std::path::{Path, PathBuf};

use grammers_session::defs::PeerRef;
use sevenz_rust2::{ArchiveEntry, ArchiveReader, ArchiveWriter, Password};

use crate::botapi::BotApi;
use crate::config::{chmod_private, Config};
use crate::mtproto::Tg;
use crate::store::Store;
use crate::{Error, Result};

/// Archive names in General look like `verity-backup-<unix>.7z`;
/// restore picks the newest one it can find.
const ARCHIVE_PREFIX: &str = "verity-backup-";
const ARCHIVE_EXT: &str = ".7z";

/// Pre-archive backups shipped these as loose documents — still
/// restorable so an old General backup isn't stranded.
const LEGACY_FILES: &[&str] = &["config.toml", "state.sqlite", "tg.session"];

/// Live sqlite dbs — a raw copy while `serve` writes can tear, so these
/// go through `VACUUM INTO` first.
const SQLITE_FILES: &[&str] = &["state.sqlite", "tg.session"];

/// Archive entries under this root land back in real `$HOME` on
/// restore — settings that live outside `~/verity` travel along.
const HOME_ROOT: &str = "_home/";

/// `$HOME`-relative dirs whose `verity*`-named files ride into
/// `HOME_ROOT`: the units that run serve/watch/keepalive and the
/// sent-file state that shouldn't re-fire after a restore.
const HOME_PARTS: &[&str] = &[".config/systemd/user", ".local/state"];

const CAPTION: &str = "🗄 verity backup — full 7z archive. `verity restore` to unpack.";
/// How far back a restore scans the channel for backup docs.
const RESTORE_SCAN: usize = 500;

/// Never shipped: build output, sqlite sidecars, restore temp files.
fn skip(path: &Path, name: &str) -> bool {
    (path.is_dir() && name == "target")
        || name.ends_with("-wal")
        || name.ends_with("-shm")
        || name.ends_with(".part")
        || (name.starts_with(ARCHIVE_PREFIX) && name.ends_with(ARCHIVE_EXT))
}

/// 7z `~/verity` (sans `target/` and friends) plus the `HOME_PARTS`
/// overlay, send the archive to General. Returns the archive name.
pub async fn upload(api: &BotApi, group_id: i64) -> Result<Vec<String>> {
    let home = Config::home()?;
    let stage = std::env::temp_dir().join(format!(
        "verity-backup-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&stage)?;
    let out = async {
        // Consistent copies of the live dbs, staged beside the archive.
        let mut snaps: Vec<(String, PathBuf)> = Vec::new();
        for name in SQLITE_FILES {
            let live = home.join(name);
            if live.exists() {
                snaps.push(((*name).to_string(), snapshot(&live, name).await?));
            }
        }
        let name = format!(
            "{ARCHIVE_PREFIX}{}{ARCHIVE_EXT}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        let archive = stage.join(&name);
        let (home2, archive2) = (home.clone(), archive.clone());
        let user_home = dirs::home_dir();
        tokio::task::spawn_blocking(move || {
            pack(&home2, user_home.as_deref(), &snaps, &archive2)
        })
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
        api.send_document(group_id, &archive, Some(CAPTION), None)
            .await?;
        Ok::<_, Error>(vec![name])
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    out
}

/// Walk `home` and write every regular file into `archive`; `snaps`
/// substitutes consistent db copies, `$HOME`'s verity bits land under
/// `_home/`. Pure Rust 7z so a restore machine needs no system tools.
fn pack(
    home: &Path,
    user_home: Option<&Path>,
    snaps: &[(String, PathBuf)],
    archive: &Path,
) -> Result<()> {
    let mut sz = ArchiveWriter::create(archive).map_err(sz_err)?;
    sz.set_encrypt_header(false);
    let mut stack = vec![home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let ft = e.file_type()?;
            if skip(&path, &name) {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue; // symlinks and sockets don't archive
            }
            let rel = path
                .strip_prefix(home)
                .map_err(|e| Error::Other(e.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            let reader = snaps
                .iter()
                .find(|(n, _)| n == &rel)
                .map(|(_, p)| p.as_path())
                .unwrap_or(&path);
            sz.push_archive_entry(
                ArchiveEntry::new_file(&rel),
                Some(std::fs::File::open(reader)?),
            )
            .map_err(sz_err)?;
        }
    }
    if let Some(user_home) = user_home {
        for part in HOME_PARTS {
            let dir = user_home.join(part);
            if !dir.is_dir() {
                continue;
            }
            for e in std::fs::read_dir(&dir)? {
                let e = e?;
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("verity") || !e.file_type()?.is_file() {
                    continue;
                }
                sz.push_archive_entry(
                    ArchiveEntry::new_file(&format!("{HOME_ROOT}{part}/{name}")),
                    Some(std::fs::File::open(e.path())?),
                )
                .map_err(sz_err)?;
            }
        }
    }
    sz.finish().map_err(|e| Error::Other(e.to_string()))?;
    Ok(())
}

fn sz_err(e: sevenz_rust2::Error) -> Error {
    Error::Other(e.to_string())
}

/// `VACUUM INTO` a uniquely-named staging dir — keeps the archived file
/// name clean and stops two backups racing on one temp file. The brain
/// goes through [`Store`] (runs migrations); the MTProto session db is
/// grammers' schema, opened raw.
async fn snapshot(db: &Path, file_name: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "verity-backup-{}-{}",
        file_name,
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

/// Pull the newest backup out of General back into `~/verity`,
/// overwriting what's there. Prefers a `verity-backup-*.7z` archive;
/// falls back to the legacy loose-file layout. Returns names restored.
pub async fn download(tg: &Tg, peer: PeerRef) -> Result<Vec<String>> {
    if let Some(msg) = tg
        .find_general_document(peer, |n| {
            n.starts_with(ARCHIVE_PREFIX) && n.ends_with(ARCHIVE_EXT)
        })
        .await?
    {
        return unpack(msg).await;
    }
    let docs = tg
        .find_general_documents(peer, LEGACY_FILES, RESTORE_SCAN)
        .await?;
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

/// Download one archive doc and extract it: plain entries into
/// `~/verity`, `_home/*` entries back into the real `$HOME`.
async fn unpack(msg: grammers_client::types::Message) -> Result<Vec<String>> {
    let stage = std::env::temp_dir().join(format!(
        "verity-restore-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&stage)?;
    let archive = stage.join("backup.7z");
    msg.download_media(&archive)
        .await
        .map_err(|e| Error::Mtproto(format!("download backup: {e}")))?;
    let home = Config::home()?;
    std::fs::create_dir_all(&home)?;
    let user_home = dirs::home_dir();
    let out = tokio::task::spawn_blocking(move || extract(&archive, &home, user_home.as_deref()))
        .await
        .map_err(|e| Error::Other(e.to_string()))?;
    let _ = std::fs::remove_dir_all(&stage);
    out
}

fn extract(archive: &Path, home: &Path, user_home: Option<&Path>) -> Result<Vec<String>> {
    let mut reader = ArchiveReader::open(archive, Password::empty()).map_err(sz_err)?;
    let mut got = Vec::new();
    reader
        .for_each_entries(|entry, r| {
            let name = entry.name().replace('\\', "/");
            if entry.is_directory() || name.is_empty() {
                return Ok(true);
            }
            // `_home/<rel>` → real $HOME; everything else → ~/verity.
            // Reject `..`/absolute paths outright — never write outside.
            let rel = Path::new(&name);
            if rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir))
            {
                return Ok(true);
            }
            let dest = match name.strip_prefix(HOME_ROOT) {
                Some(rest) => match &user_home {
                    Some(h) => h.join(rest),
                    None => return Ok(true),
                },
                None => home.join(rel),
            };
            let mut data = Vec::new();
            if std::io::Read::read_to_end(r, &mut data).is_err() {
                return Ok(true); // skip undecodable entries, keep going
            }
            if let Some(p) = dest.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            // Same .part → rename dance as legacy restore for the live
            // session/db files; drop stale sidecars with them.
            if dest.parent() == Some(home) && SQLITE_FILES.contains(&name.as_str()) {
                let tmp = home.join(format!("{name}.part"));
                std::fs::write(&tmp, &data)?;
                std::fs::rename(&tmp, &dest)?;
                for ext in ["-wal", "-shm"] {
                    let _ = std::fs::remove_file(home.join(format!("{name}{ext}")));
                }
                chmod_private(&dest);
            } else {
                std::fs::write(&dest, &data)?;
                if LEGACY_FILES.contains(&name.as_str()) {
                    chmod_private(&dest);
                }
            }
            got.push(name);
            Ok(true)
        })
        .map_err(sz_err)?;
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pack + extract round-trip: target/-wal/.part skipped, _home
    /// entries land under $HOME, path traversal entries are refused.
    #[test]
    fn archive_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("verity-bktest-{}", std::process::id()));
        let home = tmp.join("verity");
        let user_home = tmp.join("userhome");
        for d in [
            home.join("sub"),
            home.join("target"),
            user_home.join(".config/systemd/user"),
        ] {
            std::fs::create_dir_all(&d).unwrap();
        }
        std::fs::write(home.join("config.toml"), "group_id = 1").unwrap();
        std::fs::write(home.join("state.sqlite"), "db").unwrap();
        std::fs::write(home.join("state.sqlite-wal"), "sidecar").unwrap();
        std::fs::write(home.join("sub/x.txt"), "x").unwrap();
        std::fs::write(home.join("target/big.bin"), "build output").unwrap();
        std::fs::write(home.join("x.part"), "temp").unwrap();
        std::fs::write(
            user_home.join(".config/systemd/user/verity.service"),
            "[Unit]",
        )
        .unwrap();

        let archive = tmp.join("verity-backup-1.7z");
        pack(&home, Some(&user_home), &[], &archive).unwrap();

        // _home overlay and a hostile entry, added by hand.
        {
            let mut sz = ArchiveWriter::create(tmp.join("extra.7z")).unwrap();
            sz.set_encrypt_header(false);
            sz.push_archive_entry(
                ArchiveEntry::new_file("_home/.config/systemd/user/verity.service"),
                Some("[Unit]".as_bytes()),
            )
            .unwrap();
            sz.push_archive_entry(
                ArchiveEntry::new_file("../escape.txt"),
                Some("no".as_bytes()),
            )
            .unwrap();
            sz.finish().unwrap();
        }

        let out = tmp.join("out");
        let out_home = tmp.join("out-home");
        let got = extract(&archive, &out, Some(&out_home)).unwrap();
        assert!(got.contains(&"config.toml".to_string()));
        assert!(got.contains(&"sub/x.txt".to_string()));
        assert!(got.contains(&"state.sqlite".to_string()));
        assert!(!got.iter().any(|n| n.contains("target")));
        assert!(!got.iter().any(|n| n.ends_with("-wal") || n.ends_with(".part")));
        assert_eq!(
            std::fs::read_to_string(out.join("sub/x.txt")).unwrap(),
            "x"
        );
        assert!(!out.join("target/big.bin").exists());
        assert!(!out.join("state.sqlite-wal").exists());
        // _home overlay lands under the restore machine's $HOME.
        assert_eq!(
            std::fs::read_to_string(
                out_home.join(".config/systemd/user/verity.service")
            )
            .unwrap(),
            "[Unit]"
        );

        // Hostile entries are refused.
        let hostile = extract(&tmp.join("extra.7z"), &out, Some(&out_home)).unwrap();
        assert!(!tmp.join("escape.txt").exists());
        assert!(!hostile.iter().any(|n| n.contains("..")));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
