//! Self-update: `verity self-update` — fetch `origin/main`, fast-forward
//! the working tree, rebuild the release binaries, then recycle the bridge
//! so it picks up the new code. systemd's `try-restart` is used when
//! available; otherwise a restart request is left for the portable
//! `verity supervise` process. `verity-self-update.timer` can still run it
//! every ~15 min, and `verity supervise --self-update` provides the same
//! cadence without systemd.
//!
//! Unlike a shell script, the running binary can't be rewritten out
//! from under itself by the pull on unix-like systems — the kernel pins
//! the old inode — so there is no re-exec dance here.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::{Error, Result};

fn log(msg: &str) {
    println!("[self-update] {msg}");
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?)
}

fn git_ok(repo: &Path, args: &[&str]) -> Result<bool> {
    Ok(git(repo, args)?.status.success())
}

fn git_stdout(repo: &Path, args: &[&str]) -> Result<String> {
    let out = git(repo, args)?;
    if !out.status.success() {
        return Err(Error::Other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// One self-update pass. Returns Ok on a clean skip (offline, another
/// git op in flight, already current, local commits ahead) and Err on a
/// real failure — diverged history, refused fast-forward, broken build,
/// failed restart.
pub fn run() -> Result<()> {
    let repo = match std::env::var("VERITY_REPO") {
        Ok(d) => PathBuf::from(d),
        Err(_) => Config::home()?,
    };

    // Never touch the tree mid-rebase / mid-merge / mid-cherry-pick.
    let git_dir = git_stdout(&repo, &["rev-parse", "--git-dir"])?;
    // join() keeps an absolute --git-dir absolute; relative resolves
    // against the repo, which is where git means it.
    let git_dir = repo.join(&git_dir);
    for f in [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
    ] {
        if git_dir.join(f).exists() {
            log(&format!("{f} present — another git op in flight, skipping"));
            return Ok(());
        }
    }

    if !git_ok(&repo, &["fetch", "--quiet", "origin"])? {
        log("fetch failed — offline?");
        return Ok(());
    }

    let local = git_stdout(&repo, &["rev-parse", "HEAD"])?;
    let remote = git_stdout(&repo, &["rev-parse", "origin/main"])?;
    if local == remote {
        return Ok(());
    }
    if git_ok(&repo, &["merge-base", "--is-ancestor", &remote, "HEAD"])? {
        log("local commits ahead of origin/main — nothing to pull");
        return Ok(());
    }
    if !git_ok(&repo, &["merge-base", "--is-ancestor", "HEAD", &remote])? {
        return Err(Error::Other(
            "diverged from origin/main — leaving it for a human".into(),
        ));
    }

    log(&format!(
        "updating {} → {}",
        &local[..8.min(local.len())],
        &remote[..8.min(remote.len())]
    ));
    // ff-only never merges and git refuses to touch files with local
    // edits, so in-flight work in the tree survives; a conflict just
    // skips this round.
    if !git_ok(&repo, &["merge", "--ff-only", &remote])? {
        return Err(Error::Other(
            "ff-only refused (dirty tree over touched files) — skipping".into(),
        ));
    }

    // Inherit stdio so the build output lands in the journal too.
    if !Command::new("cargo")
        .args(["build", "--release", "-p", "verity-cli", "-p", "verity-gui"])
        .current_dir(&repo)
        .status()?
        .success()
    {
        return Err(Error::Other(
            "build failed — old binary keeps running".into(),
        ));
    }

    // systemd gets the first chance because try-restart only touches an
    // active bridge. Without systemd (or without the unit), leave a restart
    // request for the portable supervisor instead of failing the update.
    let systemd_restarted = Command::new("systemctl")
        .args(["--user", "try-restart", "verity.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if systemd_restarted {
        log(&format!(
            "bridge restarted on {}",
            git_stdout(&repo, &["rev-parse", "--short", "HEAD"])?
        ));
    } else {
        crate::supervisor::request_restart()?;
        log("no systemd restart — requested a portable supervisor restart");
    }
    Ok(())
}
