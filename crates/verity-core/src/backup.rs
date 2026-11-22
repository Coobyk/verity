//! `~/verity` backup into the group's General topic — one 7z archive
//! carrying the whole install (minus `target/`) plus a `_home/` overlay
//! for settings that live outside it (systemd units, per-bot send
//! state). `verity restore` pulls the newest archive back and unpacks.
//!
//! The upload carries `tg.session` (full account access) and
//! `state.sqlite` (every bot token) — it's only as private as the group.

use std::io::{Read, Seek};
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

/// Top-level entry carrying the backup machine's `$HOME`; on a
/// different-home restore, `_home` text files (systemd units with
/// absolute paths) get that string rewritten to the local one.
const SRC_HOME_MARK: &str = "_src-home";

/// The one `target/` file that does ship: a same-arch restore machine
/// then runs `serve` with no toolchain at all.
const SERVE_BIN: &str = "target/release/verity";

/// `$HOME`-relative dirs whose `verity*`-named files ride into
/// `HOME_ROOT`: the units that run serve/watch/keepalive and the
/// sent-file state that shouldn't re-fire after a restore.
const HOME_PARTS: &[&str] = &[".config/systemd/user", ".local/state"];

const CAPTION: &str =
    "🗄 verity backup — full 7z archive. Save it and `verity restore <path>` on a new machine.";
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
        let mut snaps = Vec::new();
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
        tokio::task::spawn_blocking(move || pack(&home2, user_home.as_deref(), &snaps, &archive2))
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
    snaps: &[(String, Snapshot)],
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
                .map(|(_, p)| p.path.as_path())
                .unwrap_or(&path);
            sz.push_archive_entry(
                ArchiveEntry::new_file(&rel),
                Some(std::fs::File::open(reader)?),
            )
            .map_err(sz_err)?;
        }
    }
    // `target/` is skipped wholesale, but the release binary itself
    // rides along — without it a fresh machine couldn't start serve.
    let bin = home.join(SERVE_BIN);
    if bin.is_file() {
        sz.push_archive_entry(
            ArchiveEntry::new_file(SERVE_BIN),
            Some(std::fs::File::open(&bin)?),
        )
        .map_err(sz_err)?;
    }
    if let Some(user_home) = user_home {
        sz.push_archive_entry(
            ArchiveEntry::new_file(SRC_HOME_MARK),
            Some(user_home.to_string_lossy().as_bytes()),
        )
        .map_err(sz_err)?;
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
struct Snapshot {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for Snapshot {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

async fn snapshot(db: &Path, file_name: &str) -> Result<Snapshot> {
    let dir = tempfile::Builder::new()
        .prefix(&format!("verity-backup-{file_name}-"))
        .tempdir()?;
    let snap = Snapshot {
        path: dir.path().join(file_name),
        _dir: dir,
    };
    let db = db.to_path_buf();
    let via_store = file_name == "state.sqlite";
    tokio::task::spawn_blocking(move || -> Result<Snapshot> {
        if via_store {
            Store::open(&db)?.vacuum_into(&snap.path)?;
        } else {
            let conn = sqlite::Connection::open(&db)?;
            conn.execute("PRAGMA busy_timeout = 5000")?;
            let esc = snap.path.to_string_lossy().replace('\'', "''");
            conn.execute(format!("VACUUM INTO '{esc}'"))?;
        }
        Ok(snap)
    })
    .await
    .map_err(|e| Error::Other(e.to_string()))?
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
    let user_home = dirs::home_dir();
    let out = tokio::task::spawn_blocking(move || extract(&archive, &home, user_home.as_deref()))
        .await
        .map_err(|e| Error::Other(e.to_string()))?;
    let _ = std::fs::remove_dir_all(&stage);
    out
}

/// `verity restore <archive>` — point at a local `verity-backup-*.7z`
/// or an https URL and it unpacks itself. No Telegram session is
/// needed: the session rides inside the archive, which is the whole
/// point on a machine that was never set up. Returns names restored.
pub async fn restore_arg(arg: &str) -> Result<Vec<String>> {
    if arg.starts_with("http://") || arg.starts_with("https://") {
        let stage = std::env::temp_dir().join(format!(
            "verity-restore-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&stage)?;
        let archive = stage.join("backup.7z");
        let out = async {
            fetch(arg, &archive).await?;
            let home = Config::home()?;
            let user_home = dirs::home_dir();
            tokio::task::spawn_blocking(move || extract(&archive, &home, user_home.as_deref()))
                .await
                .map_err(|e| Error::Other(e.to_string()))?
        }
        .await;
        let _ = std::fs::remove_dir_all(&stage);
        return out;
    }
    let path = expand_tilde(arg);
    if !path.is_file() {
        return Err(Error::Config(format!(
            "no archive at \"{arg}\" — pass a {ARCHIVE_PREFIX}*{ARCHIVE_EXT} path or https URL \
             (to scan General instead: `verity restore --group <id>`)"
        )));
    }
    let home = Config::home()?;
    let user_home = dirs::home_dir();
    tokio::task::spawn_blocking(move || extract(&path, &home, user_home.as_deref()))
        .await
        .map_err(|e| Error::Other(e.to_string()))?
}

/// `~/x` → `$HOME/x`; everything else passes through untouched.
fn expand_tilde(arg: &str) -> PathBuf {
    match arg.strip_prefix("~/") {
        Some(rest) => match dirs::home_dir() {
            Some(h) => h.join(rest),
            None => PathBuf::from(arg),
        },
        None => PathBuf::from(arg),
    }
}

async fn fetch(url: &str, dest: &Path) -> Result<()> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| Error::Other(format!("fetch {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("fetch {url}: {}", resp.status())));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::Other(format!("fetch {url}: {e}")))?;
    std::fs::write(dest, &bytes)?;
    Ok(())
}

/// Light up what a restore just unpacked: reload systemd --user and
/// `enable --now` every unit that came back (skipping ones whose
/// ExecStart binary isn't there), plus linger so serve survives
/// logout. All best-effort — every step reports into the notes.
pub fn post_restore(files: &[String]) -> Vec<String> {
    const UNITS: &str = "_home/.config/systemd/user/";
    let mut notes = Vec::new();
    let units: Vec<&str> = files
        .iter()
        .filter_map(|n| n.strip_prefix(UNITS))
        .filter(|n| n.ends_with(".service") || n.ends_with(".timer"))
        .collect();
    if units.is_empty() {
        return notes;
    }
    let Some(uhome) = dirs::home_dir() else {
        return notes;
    };
    if run_status("systemctl", &["--user", "daemon-reload"]) {
        notes.push("systemd --user reloaded".to_string());
    } else {
        notes.push("systemctl --user daemon-reload failed — enable units by hand".to_string());
        return notes;
    }
    for u in units {
        let unit_file = uhome.join(".config/systemd/user").join(u);
        if let Some(missing) = missing_exec(&unit_file) {
            notes.push(format!(
                "{u}: not started — {missing} doesn't exist \
                 (build/install it, then `systemctl --user enable --now {u}`)"
            ));
            continue;
        }
        notes.push(format!(
            "{u}: {}",
            if run_status("systemctl", &["--user", "enable", "--now", u]) {
                "enabled + started"
            } else {
                "enable failed — try `systemctl --user enable --now` by hand"
            }
        ));
    }
    if run_status("loginctl", &["enable-linger"]) {
        notes.push("linger on — serve survives logout".to_string());
    }
    notes
}

fn run_status(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// First token of a unit's ExecStart; `Some(path)` when it's absolute
/// and doesn't exist — enabling a service like that just restart-loops.
fn missing_exec(unit: &Path) -> Option<String> {
    let text = std::fs::read_to_string(unit).ok()?;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("ExecStart=") {
            let bin = v.split_whitespace().next()?;
            if bin.starts_with('/') && !Path::new(bin).exists() {
                return Some(bin.to_string());
            }
        }
    }
    None
}

fn extract(archive: &Path, home: &Path, user_home: Option<&Path>) -> Result<Vec<String>> {
    let mut reader = ArchiveReader::open(archive, Password::empty()).map_err(sz_err)?;
    // The multithreaded LZMA2 decoder buffers entire decompressed work units.
    reader.set_thread_count(1);
    let mut files = Vec::new();
    let mut changes = Vec::new();
    let mut home_entries = Vec::new();
    let mut src_home: Option<String> = None;
    reader
        .for_each_entries(|entry, r| {
            let name = entry.name().replace('\\', "/");
            if entry.is_directory() || name.is_empty() {
                return Ok(true);
            }
            let rel = Path::new(name.strip_prefix(HOME_ROOT).unwrap_or(&name));
            let read_error =
                |e| sevenz_rust2::Error::Io(e, format!("read archive entry {name}").into());
            if rel.as_os_str().is_empty()
                || rel.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                std::io::copy(r, &mut std::io::sink()).map_err(read_error)?;
                return Ok(true);
            }
            if name == SRC_HOME_MARK {
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut r.take(4096), &mut data).map_err(read_error)?;
                std::io::copy(r, &mut std::io::sink()).map_err(read_error)?;
                src_home = String::from_utf8(data).ok().map(|s| s.trim().to_string());
            } else if name.starts_with(HOME_ROOT) {
                if user_home.is_some() {
                    let dir = restore_staging_dir(&std::env::temp_dir()).map_err(read_error)?;
                    let mut raw = tempfile::tempfile_in(dir.path()).map_err(read_error)?;
                    std::io::copy(r, &mut raw).map_err(read_error)?;
                    home_entries.push((name, raw, dir));
                } else {
                    std::io::copy(r, &mut std::io::sink()).map_err(read_error)?;
                }
            } else {
                changes.push(
                    RestoreChange::stage(home.join(rel), r, &name).map_err(|e| {
                        sevenz_rust2::Error::Io(e, format!("stage archive entry {name}").into())
                    })?,
                );
                if SQLITE_FILES.contains(&name.as_str()) {
                    for ext in ["-wal", "-shm"] {
                        changes.push(RestoreChange {
                            dest: home.join(format!("{name}{ext}")),
                            replacement: None,
                        });
                    }
                }
                files.push(name);
            }
            Ok(true)
        })
        .map_err(sz_err)?;
    if let Some(uhome) = user_home {
        let new_home = uhome.to_string_lossy();
        for (name, mut raw, _dir) in home_entries {
            raw.rewind()?;
            let dest = uhome.join(&name[HOME_ROOT.len()..]);
            let change = match &src_home {
                Some(old) if !old.is_empty() && *old != new_home => {
                    let mut data = Vec::new();
                    raw.read_to_end(&mut data)?;
                    let text = String::from_utf8_lossy(&data).replace(old.as_str(), &new_home);
                    RestoreChange::stage(dest, text.as_bytes(), &name)?
                }
                _ => RestoreChange::stage(dest, &mut raw, &name)?,
            };
            changes.push(change);
            files.push(name);
        }
    }
    apply_restore(changes)?;
    Ok(files)
}

struct RestoreChange {
    dest: PathBuf,
    replacement: Option<StagedRestore>,
}

struct StagedRestore {
    file: tempfile::TempPath,
    _dir: tempfile::TempDir,
}

fn restore_staging_dir(parent: &Path) -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.prefix(".verity-restore-").tempdir_in(parent)
}

impl RestoreChange {
    fn stage(dest: PathBuf, mut reader: impl std::io::Read, name: &str) -> std::io::Result<Self> {
        let dest = std::path::absolute(dest)?;
        // Stage on the destination filesystem, without creating live directories.
        let parent = dest
            .ancestors()
            .skip(1)
            .find(|p| p.is_dir())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no staging directory")
            })?;
        let dir = restore_staging_dir(parent)?;
        let mut builder = tempfile::Builder::new();
        let original_mode = std::fs::symlink_metadata(&dest)
            .ok()
            .filter(|meta| meta.is_file())
            .map(|meta| meta.permissions());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if LEGACY_FILES.contains(&name) || original_mode.is_some() {
                0o600
            } else {
                0o666
            };
            builder.permissions(std::fs::Permissions::from_mode(mode));
        }
        let mut staged = builder.tempfile_in(dir.path())?;
        std::io::copy(&mut reader, &mut staged)?;
        if name == SERVE_BIN {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                staged
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o755))?;
            }
        } else if LEGACY_FILES.contains(&name) {
            chmod_private(staged.path());
        } else if let Some(mode) = original_mode {
            staged.as_file().set_permissions(mode)?;
        }
        Ok(Self {
            dest,
            replacement: Some(StagedRestore {
                file: staged.into_temp_path(),
                _dir: dir,
            }),
        })
    }

    fn apply(self, dirs: &mut Vec<PathBuf>, undo: &mut Vec<RestoreUndo>) -> std::io::Result<()> {
        let parent = self
            .dest
            .parent()
            .expect("restore destination has a parent");
        let original = match std::fs::symlink_metadata(&self.dest) {
            Ok(meta) if meta.is_file() || meta.is_symlink() => {
                let backup = restore_staging_dir(parent)?;
                std::fs::rename(&self.dest, backup.path().join("original"))?;
                Some(backup)
            }
            Ok(_) => {
                return Err(std::io::Error::other(
                    "restore destination is not a regular file",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let had_original = original.is_some();
        if had_original {
            undo.push(RestoreUndo {
                dest: self.dest.clone(),
                original,
            });
        }
        if let Some(staged) = self.replacement {
            create_restore_dirs(parent, dirs)?;
            staged.file.persist(&self.dest).map_err(|e| e.error)?;
            if !had_original {
                undo.push(RestoreUndo {
                    dest: self.dest,
                    original: None,
                });
            }
        }
        Ok(())
    }
}

struct RestoreUndo {
    dest: PathBuf,
    original: Option<tempfile::TempDir>,
}

impl RestoreUndo {
    fn rollback(self) -> Result<()> {
        if let Some(backup) = self.original {
            if let Err(e) = std::fs::rename(backup.path().join("original"), &self.dest) {
                return Err(Error::Other(format!(
                    "{}: {e}; previous file kept in {}",
                    self.dest.display(),
                    backup.keep().display()
                )));
            }
        } else {
            std::fs::remove_file(&self.dest)?;
        }
        Ok(())
    }
}

fn create_restore_dirs(path: &Path, created: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_restore_dirs(parent, created)?;
    }
    std::fs::create_dir(path)?;
    created.push(path.to_path_buf());
    Ok(())
}

fn apply_restore(changes: Vec<RestoreChange>) -> Result<()> {
    let mut undo = Vec::new();
    let mut dirs = Vec::new();
    for change in changes {
        let dest = change.dest.clone();
        if let Err(e) = change.apply(&mut dirs, &mut undo) {
            let mut error = format!("cannot restore {}: {e}", dest.display());
            for original in undo.into_iter().rev() {
                if let Err(e) = original.rollback() {
                    error.push_str(&format!("; rollback failed: {e}"));
                }
            }
            for dir in dirs.into_iter().rev() {
                if let Err(e) = std::fs::remove_dir(&dir) {
                    error.push_str(&format!("; could not remove {}: {e}", dir.display()));
                }
            }
            return Err(Error::Other(error));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_is_removed_after_use() {
        let db = std::env::temp_dir().join(format!(
            "verity-snapshot-test-{}.sqlite",
            std::process::id()
        ));
        Store::open(&db).unwrap().kv_set("test", "saved").unwrap();
        let snap = snapshot(&db, "state.sqlite").await.unwrap();
        let path = snap.to_path_buf();
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .kv_get("test")
                .unwrap()
                .as_deref(),
            Some("saved")
        );
        drop(snap);
        let removed = !path.exists() && !path.parent().unwrap().exists();
        let _ = std::fs::remove_file(&db);
        assert!(
            removed,
            "snapshot staging directory leaked: {}",
            path.display()
        );
    }

    #[tokio::test]
    async fn snapshot_raw_empty_database_and_error_cleanup() {
        let source = tempfile::tempdir().unwrap();
        let db = source.path().join("tg.session");
        sqlite::Connection::open(&db).unwrap();
        let snap = snapshot(&db, "tg.session").await.unwrap();
        let dir = snap.parent().unwrap().to_path_buf();
        assert!(snap.is_file());
        drop(snap);
        assert!(!dir.exists());
        std::fs::write(&db, "not sqlite").unwrap();
        assert!(snapshot(&db, "tg.session").await.is_err());
    }

    #[test]
    fn corrupt_database_entry_leaves_restore_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let db = home.join("state.sqlite");
        let config = home.join("config.toml");
        std::fs::write(&config, "group_id = 1").unwrap();
        Store::open(&db)
            .unwrap()
            .kv_set("checkpoint", "old")
            .unwrap();
        let old_db = std::fs::read(&db).unwrap();
        let new_db = home.join("new.sqlite");
        let marker = b"NEW_RESTORE_CHECKPOINT";
        Store::open(&new_db)
            .unwrap()
            .kv_set("checkpoint", std::str::from_utf8(marker).unwrap())
            .unwrap();

        let archive = home.join("corrupt.7z");
        let mut writer = ArchiveWriter::create(&archive).unwrap();
        writer.set_content_methods(vec![sevenz_rust2::EncoderMethod::COPY.into()]);
        writer
            .push_archive_entry(
                ArchiveEntry::new_file("config.toml"),
                Some(&b"group_id = 2"[..]),
            )
            .unwrap();
        writer
            .push_archive_entry(
                ArchiveEntry::new_file("state.sqlite"),
                Some(std::fs::File::open(new_db).unwrap()),
            )
            .unwrap();
        writer.finish().unwrap();
        let mut bytes = std::fs::read(&archive).unwrap();
        let offset = bytes
            .windows(marker.len())
            .position(|b| b == marker)
            .unwrap();
        bytes[offset] ^= 1;
        std::fs::write(&archive, bytes).unwrap();

        let result = extract(&archive, home, None).map_err(|e| e.to_string());
        assert_eq!(
            (
                result.as_ref().is_err_and(|e| e.contains("state.sqlite")),
                std::fs::read_to_string(config).unwrap().as_str(),
                std::fs::read(db).unwrap() == old_db,
            ),
            (true, "group_id = 1", true),
            "restore returned {result:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn restore_stages_overlay_in_private_directory() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("verity.service");
        std::fs::write(&dest, b"old").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).unwrap();
        let change = RestoreChange::stage(dest, &b"new"[..], "_home/verity.service").unwrap();
        let staged = change.replacement.unwrap();
        let mode = |path: &Path| path.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(staged._dir.path()), 0o700);
        assert_eq!(mode(&staged.file), 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn archive_restore_replaces_database_symlink_or_restores_it_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("verity");
        std::fs::create_dir(&home).unwrap();
        let db = home.join("state.sqlite");
        let target = tmp.path().join("external.sqlite");
        std::fs::write(&target, "external db").unwrap();
        std::os::unix::fs::symlink("../external.sqlite", &db).unwrap();
        let blocked = tmp.path().join("verity.service");
        std::fs::create_dir(&blocked).unwrap();
        let archive = tmp.path().join("backup.7z");
        let mut writer = ArchiveWriter::create(&archive).unwrap();
        for (name, data) in [
            ("state.sqlite", "new db"),
            ("_home/verity.service", "new unit"),
        ] {
            writer
                .push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_bytes()))
                .unwrap();
        }
        writer.finish().unwrap();

        let error = extract(&archive, &home, Some(tmp.path())).unwrap_err();
        assert!(error.to_string().contains("verity.service"), "{error}");
        assert_eq!(
            std::fs::read_link(&db).unwrap(),
            Path::new("../external.sqlite")
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "external db");

        extract(&archive, &home, None).unwrap();
        assert!(std::fs::symlink_metadata(&db).unwrap().is_file());
        assert_eq!(std::fs::read_to_string(&db).unwrap(), "new db");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "external db");
    }

    #[test]
    fn failed_persist_puts_back_the_original_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("config.toml");
        std::fs::write(&dest, "old config").unwrap();
        let change = RestoreChange::stage(dest.clone(), &b"new config"[..], "config.toml").unwrap();
        std::fs::remove_file(&change.replacement.as_ref().unwrap().file).unwrap();

        let error = apply_restore(vec![change]).unwrap_err();
        assert!(error.to_string().contains("config.toml"), "{error}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old config");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_archive_install_removes_a_new_restore_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("fresh").join("verity");
        let blocked = tmp.path().join("verity.service");
        std::fs::create_dir(&blocked).unwrap();
        let archive = tmp.path().join("backup.7z");
        let mut writer = ArchiveWriter::create(&archive).unwrap();
        for (name, data) in [
            ("sub/config.toml", "new config"),
            ("_home/verity.service", "new unit"),
        ] {
            writer
                .push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_bytes()))
                .unwrap();
        }
        writer.finish().unwrap();

        let error = extract(&archive, &home, Some(tmp.path())).unwrap_err();
        assert!(error.to_string().contains("verity.service"), "{error}");
        assert!(!error.to_string().contains("rollback failed"), "{error}");
        assert!(!home.parent().unwrap().exists());
        assert!(blocked.is_dir());
    }

    #[test]
    fn failed_archive_install_rolls_back_files_and_sqlite_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("verity");
        let user_home = tmp.path().join("user-home");
        std::fs::create_dir(&home).unwrap();
        let blocked = user_home.join(".config/systemd/user/verity.service");
        std::fs::create_dir_all(&blocked).unwrap();
        let originals = [
            ("config.toml", "old config"),
            ("state.sqlite", "old db"),
            ("state.sqlite-wal", "old wal"),
            ("state.sqlite-shm", "old shm"),
        ];
        for (name, data) in originals {
            std::fs::write(home.join(name), data).unwrap();
        }
        let archive = tmp.path().join("backup.7z");
        let mut writer = ArchiveWriter::create(&archive).unwrap();
        for (name, data) in [
            ("config.toml", "new config"),
            ("state.sqlite", "new db"),
            ("new-dir/file", "new file"),
            ("_home/.config/systemd/user/verity.service", "new unit"),
        ] {
            writer
                .push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_bytes()))
                .unwrap();
        }
        writer.finish().unwrap();

        let error = extract(&archive, &home, Some(&user_home)).unwrap_err();
        assert!(error.to_string().contains("verity.service"), "{error}");
        assert!(!error.to_string().contains("rollback failed"), "{error}");
        for (name, data) in originals {
            assert_eq!(std::fs::read_to_string(home.join(name)).unwrap(), data);
        }
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), originals.len());
        assert!(blocked.is_dir());
    }

    #[test]
    #[cfg(unix)]
    fn home_rewrite_preserves_write_only_destination_permissions() {
        use std::os::unix::fs::PermissionsExt;

        if std::process::Command::new("id")
            .arg("-u")
            .output()
            .unwrap()
            .stdout
            == b"0\n"
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("backup.7z");
        let mut writer = ArchiveWriter::create(&archive).unwrap();
        for (name, data) in [
            ("_home/verity.service", "ExecStart=/old/home/verity"),
            (SRC_HOME_MARK, "/old/home"),
        ] {
            writer
                .push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_bytes()))
                .unwrap();
        }
        writer.finish().unwrap();
        let unit = tmp.path().join("verity.service");
        std::fs::write(&unit, "old unit").unwrap();
        std::fs::set_permissions(&unit, std::fs::Permissions::from_mode(0o200)).unwrap();

        extract(&archive, &tmp.path().join("verity"), Some(tmp.path())).unwrap();

        assert_eq!(unit.metadata().unwrap().permissions().mode() & 0o777, 0o200);
        std::fs::set_permissions(&unit, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&unit).unwrap(),
            format!("ExecStart={}/verity", tmp.path().display())
        );
    }

    #[test]
    fn home_marker_after_entries_rewrites_staged_files() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("backup.7z");
        let mut writer = ArchiveWriter::create(&archive).unwrap();
        for (name, data) in [
            ("_home/verity.service", "ExecStart=/old/home/verity"),
            ("config.toml", "config"),
            ("_home/empty", ""),
            (SRC_HOME_MARK, "/old/home"),
        ] {
            writer
                .push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_bytes()))
                .unwrap();
        }
        writer.finish().unwrap();
        let home = tmp.path().join("verity");
        let user_home = tmp.path().join("user");
        std::fs::create_dir(&user_home).unwrap();
        let unit = user_home.join("verity.service");
        std::fs::write(&unit, "old unit").unwrap();
        let mut permissions = unit.metadata().unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&unit, permissions).unwrap();
        let files = extract(&archive, &home, Some(&user_home)).unwrap();
        assert_eq!(files, ["config.toml", "_home/verity.service", "_home/empty"]);
        assert_eq!(
            std::fs::read_to_string(user_home.join("verity.service")).unwrap(),
            format!("ExecStart={}/verity", user_home.display())
        );
        assert_eq!(std::fs::read(user_home.join("empty")).unwrap(), b"");
        assert!(unit.metadata().unwrap().permissions().readonly());
        assert_eq!(extract(&archive, &home, None).unwrap(), ["config.toml"]);
    }

    /// pack + extract round-trip: target/-wal/.part skipped, _home
    /// entries land under $HOME, path traversal entries are refused.
    #[test]
    fn archive_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("verity-bktest-{}", std::process::id()));
        let home = tmp.join("verity");
        let user_home = tmp.join("userhome");
        for d in [
            home.join("sub"),
            home.join("target/release"),
            user_home.join(".config/systemd/user"),
        ] {
            std::fs::create_dir_all(&d).unwrap();
        }
        std::fs::write(home.join("config.toml"), "group_id = 1").unwrap();
        std::fs::write(home.join("state.sqlite"), "db").unwrap();
        std::fs::write(home.join("state.sqlite-wal"), "sidecar").unwrap();
        std::fs::write(home.join("sub/x.txt"), "x").unwrap();
        std::fs::write(home.join("target/big.bin"), "build output").unwrap();
        std::fs::write(home.join("target/release/verity"), "binary").unwrap();
        std::fs::write(home.join("x.part"), "temp").unwrap();
        std::fs::write(
            user_home.join(".config/systemd/user/verity.service"),
            format!(
                "[Service]\nExecStart={}/verity/target/release/verity serve",
                user_home.display()
            ),
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
        assert!(!got.iter().any(|n| n.contains("big.bin")));
        assert!(!got
            .iter()
            .any(|n| n.ends_with("-wal") || n.ends_with(".part")));
        assert_eq!(std::fs::read_to_string(out.join("sub/x.txt")).unwrap(), "x");
        assert!(!out.join("target/big.bin").exists());
        assert!(!out.join("state.sqlite-wal").exists());
        // The marker is consumed for the rewrite, never written out.
        assert!(!out.join(SRC_HOME_MARK).exists());
        // ...but the serve binary does ship and comes back executable.
        let bin = out.join(SERVE_BIN);
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(bin.metadata().unwrap().permissions().mode() & 0o777, 0o755);
        }
        // _home overlay lands under the restore machine's $HOME, with
        // the backup machine's home path rewritten to the local one.
        assert_eq!(
            std::fs::read_to_string(out_home.join(".config/systemd/user/verity.service")).unwrap(),
            format!(
                "[Service]\nExecStart={}/verity/target/release/verity serve",
                out_home.display()
            )
        );

        // Hostile entries are refused.
        let hostile = extract(&tmp.join("extra.7z"), &out, Some(&out_home)).unwrap();
        assert!(!tmp.join("escape.txt").exists());
        assert!(!hostile.iter().any(|n| n.contains("..")));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
