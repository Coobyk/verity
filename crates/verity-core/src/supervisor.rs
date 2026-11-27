//! Portable process supervision for `verity serve` — no systemd required.
//!
//! The supervisor owns one child running `verity serve`, restarts it after
//! crashes or `:::restart:::`, honors the asleep flag written by
//! `:::shutdown:::`, kills wedged children when their heartbeat stops, and
//! notices restart requests left by `verity self-update`. It is deliberately
//! small: operating-system autostart can still be supplied by systemd,
//! launchd, Task Scheduler, tmux, or any other launcher, but the bridge's
//! liveness no longer depends on systemd semantics.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::{liveness, Result};

const HEARTBEAT_CHECK: Duration = Duration::from_secs(10);
const RESTART_MIN: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(60);
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// Portable supervisor knobs. `serve_args` are appended after `serve` —
/// normally `--echo` and/or `--no-context`.
pub struct Options {
    pub serve_args: Vec<String>,
    /// Run `verity self-update` on this cadence. `None` disables it.
    pub self_update_every: Option<Duration>,
}

/// Run the serve child until Ctrl-C or `:::shutdown:::`. Never returns for
/// ordinary child exits — those are exactly what gets restarted.
pub async fn run(opts: Options) -> Result<()> {
    if let Some(every) = opts.self_update_every {
        spawn_self_update(every);
    }

    let mut backoff = RESTART_MIN;
    loop {
        if liveness::asleep_path().exists() {
            println!("asleep flag set — supervisor staying down");
            return Ok(());
        }

        let mut child = spawn_serve(&opts.serve_args)?;
        let pid = child.id();
        let started = Instant::now();
        info!(pid, "serve child started");

        let mut check = tokio::time::interval(HEARTBEAT_CHECK);
        // Consume the immediate first tick; the first real check lands one
        // interval in, after the child has had time to write its first beat.
        check.tick().await;

        let mut status = None;
        let mut operator_stop = false;
        let mut intentional_recycle = false;
        loop {
            tokio::select! {
                s = child.wait() => {
                    status = Some(s?);
                    break;
                }
                _ = tokio::signal::ctrl_c() => {
                    operator_stop = true;
                    let _ = child.kill().await;
                    break;
                }
                _ = check.tick() => {
                    if liveness::take_restart_request() {
                        info!("restart requested — recycling serve child");
                        intentional_recycle = true;
                        let _ = child.kill().await;
                        break;
                    }
                    if liveness::asleep_path().exists() {
                        info!("asleep flag appeared — stopping serve child");
                        intentional_recycle = true;
                        let _ = child.kill().await;
                        break;
                    }
                    if started.elapsed() > liveness::STALE_AFTER
                        && heartbeat_stale_for(pid, liveness::STALE_AFTER)
                    {
                        warn!(pid, "serve heartbeat stale — killing wedged child");
                        let _ = child.kill().await;
                        break;
                    }
                }
            }
        }

        // Reap the child when the select branch was a signal/maintenance
        // restart rather than the child's own exit.
        if status.is_none() {
            status = child.wait().await.ok();
        }
        if operator_stop {
            println!("supervisor stopping");
            return Ok(());
        }
        if liveness::asleep_path().exists() {
            println!("asleep flag set — supervisor staying down");
            return Ok(());
        }

        let uptime = started.elapsed();
        backoff = if intentional_recycle {
            RESTART_MIN
        } else {
            next_backoff(backoff, uptime)
        };
        match status {
            Some(s) => {
                warn!(status = %s, uptime_s = uptime.as_secs(), backoff_s = backoff.as_secs(), "serve child exited; restarting")
            }
            None => warn!(
                uptime_s = uptime.as_secs(),
                backoff_s = backoff.as_secs(),
                "serve child status unknown; restarting"
            ),
        }
        tokio::time::sleep(backoff).await;
    }
}

fn spawn_serve(serve_args: &[String]) -> Result<Child> {
    // VERITY_SERVE_BIN matters for self-updating installs: the supervisor
    // can itself be a copy outside target/, while every serve child comes
    // from the freshly rebuilt release binary. Windows especially needs
    // that split because a running executable cannot be overwritten.
    let exe = std::env::var("VERITY_SERVE_BIN")
        .map(PathBuf::from)
        .ok()
        .filter(|p| p.is_file())
        .map(Ok)
        .unwrap_or_else(std::env::current_exe)?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .args(serve_args)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    Ok(cmd.spawn()?)
}

fn spawn_self_update(every: Duration) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(every);
        // Consume the immediate tick so the first update happens one full
        // interval after boot, not during startup.
        timer.tick().await;
        loop {
            timer.tick().await;
            if liveness::asleep_path().exists() {
                continue;
            }
            info!("running periodic self-update");
            match tokio::task::spawn_blocking(crate::selfupdate::run).await {
                Ok(Ok(())) => info!("self-update pass finished"),
                Ok(Err(e)) => warn!(error = %e, "self-update failed"),
                Err(e) => warn!(error = %e, "self-update task failed"),
            }
        }
    });
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True when the heartbeat file belongs to `pid` and its newest beat is
/// older than `stale_after`. Missing or foreign files are not evidence of a
/// wedge — the child may still be booting, or the file may belong to another
/// serve instance.
fn heartbeat_stale_for(pid: Option<u32>, stale_after: Duration) -> bool {
    let body = match std::fs::read_to_string(liveness::heartbeat_path()) {
        Ok(b) => b,
        Err(_) => return false,
    };
    heartbeat_stale_body(&body, pid, unix_now(), stale_after)
}

fn heartbeat_stale_body(body: &str, pid: Option<u32>, now: u64, stale_after: Duration) -> bool {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    match (pid, json.get("pid").and_then(|p| p.as_u64())) {
        (Some(want), Some(got)) if want as u64 != got => return false,
        (Some(_), None) => return false,
        _ => {}
    }
    let newest = json
        .get("beats")
        .and_then(|b| b.as_object())
        .into_iter()
        .flat_map(|beats| beats.values())
        .filter_map(|v| v.as_u64())
        .max();
    newest.is_some_and(|ts| now.saturating_sub(ts) > stale_after.as_secs())
}

/// Crash backoff doubles up to a minute; a child that stayed up for a
/// healthy stretch resets the backoff because its next exit is a fresh
/// failure, not a restart loop.
fn next_backoff(previous: Duration, uptime: Duration) -> Duration {
    if uptime >= STABLE_AFTER {
        RESTART_MIN
    } else {
        (previous * 2).min(RESTART_MAX)
    }
}

/// State-file path used for portable restarts. Exposed for tests and docs.
pub fn restart_request_path() -> PathBuf {
    liveness::restart_request_path()
}

/// Ask any running portable supervisor to recycle its serve child.
pub fn request_restart() -> Result<()> {
    let path = restart_request_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, b"restart")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_stale_only_for_matching_pid() {
        let body = r#"{"pid":42,"beats":{"_boot":100,"scout":100}}"#;
        assert!(heartbeat_stale_body(
            body,
            Some(42),
            200,
            Duration::from_secs(50)
        ));
        assert!(!heartbeat_stale_body(
            body,
            Some(43),
            200,
            Duration::from_secs(50)
        ));
        assert!(!heartbeat_stale_body(
            body,
            Some(42),
            149,
            Duration::from_secs(50)
        ));
    }

    #[test]
    fn heartbeat_uses_newest_beat() {
        let body = r#"{"pid":42,"beats":{"old":1,"new":190}}"#;
        assert!(!heartbeat_stale_body(
            body,
            Some(42),
            200,
            Duration::from_secs(50)
        ));
    }

    #[test]
    fn malformed_or_missing_heartbeat_is_not_stale() {
        assert!(!heartbeat_stale_body(
            "junk",
            Some(42),
            200,
            Duration::from_secs(50)
        ));
        assert!(!heartbeat_stale_body(
            r#"{"beats":{"boot":1}}"#,
            Some(42),
            200,
            Duration::from_secs(50)
        ));
    }

    #[test]
    fn backoff_resets_after_stable_uptime() {
        assert_eq!(
            next_backoff(RESTART_MIN, Duration::from_secs(2)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(32), Duration::from_secs(2)),
            RESTART_MAX
        );
        assert_eq!(next_backoff(RESTART_MAX, STABLE_AFTER), RESTART_MIN);
    }
}
