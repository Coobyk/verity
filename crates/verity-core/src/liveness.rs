//! Liveness: `serve` keeps a heartbeat file warm; `verity watchdog`
//! (verity-watchdog.timer, once a minute) reads it and resurrects the
//! bridge — `systemctl start` when the unit is down, `restart` when the
//! heartbeat went stale. A deliberate death is honored: a disabled unit
//! or the `asleep` flag means "stay dead" (PC handoff, `:::shutdown:::`),
//! and `serve` clears the flag when it comes back up.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tracing::warn;

use crate::config::Config;
use crate::Result;

/// How often the in-process flusher persists beats to disk.
const FLUSH_EVERY: Duration = Duration::from_secs(15);
/// A newest-beat this old means the writer is dead. Comfortably above
/// the worst healthy gap: the 75 s getUpdates timeout plus a turn of
/// bookkeeping, and conflict-backoff sleeps are chunked under it.
const STALE_AFTER: Duration = Duration::from_secs(150);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state")
}

/// `~/.local/state/verity-heartbeat.json` — `{pid, beats: {who: unix}}`.
pub fn heartbeat_path() -> PathBuf {
    state_dir().join("verity-heartbeat.json")
}

/// `~/.local/state/verity-asleep` — present means "stay dead". Written
/// by `:::shutdown:::` (or by hand before a PC handoff), removed by the
/// next `serve` start.
pub fn asleep_path() -> PathBuf {
    state_dir().join("verity-asleep")
}

/// Sign-of-life registry: each bot loop and the supervisor beat under
/// their own name; the flusher persists the map for the watchdog.
#[derive(Clone, Default)]
pub struct Heartbeat {
    beats: Arc<Mutex<HashMap<String, u64>>>,
}

impl Heartbeat {
    /// Record a sign of life from `who` — a bot name or "_supervisor".
    pub fn beat(&self, who: &str) {
        self.beats
            .lock()
            .unwrap()
            .insert(who.to_string(), now());
    }

    /// Persist beats every `FLUSH_EVERY` until the process exits, and
    /// clear the asleep flag — coming up means the holiday is over.
    pub fn spawn_flusher(&self) {
        let _ = std::fs::remove_file(asleep_path());
        let path = heartbeat_path();
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        self.beat("_boot");
        let hb = self.clone();
        tokio::spawn(async move {
            loop {
                let body = json!({
                    "pid": std::process::id(),
                    "beats": hb.beats.lock().unwrap().clone(),
                });
                let _ = std::fs::write(&path, body.to_string());
                tokio::time::sleep(FLUSH_EVERY).await;
            }
        });
    }
}

async fn systemctl(args: &[&str]) -> String {
    match tokio::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .await
    {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if !e.is_empty() {
                if !s.is_empty() {
                    s.push_str(" — ");
                }
                s.push_str(&e);
            }
            s
        }
        Err(e) => format!("systemctl failed: {e}"),
    }
}

/// Newest beat in the heartbeat file, or None if it's absent/unreadable.
fn last_beat() -> Option<u64> {
    let s = std::fs::read_to_string(heartbeat_path()).ok()?;
    serde_json::from_str::<serde_json::Value>(&s)
        .ok()?
        .get("beats")?
        .as_object()?
        .values()
        .filter_map(|v| v.as_u64())
        .max()
}

/// How long the unit has been active (monotonic clock), if it is.
async fn unit_uptime(unit: &str) -> Option<Duration> {
    let out = systemctl(&[
        "show",
        unit,
        "-p",
        "ActiveEnterTimestampMonotonic",
        "--value",
    ])
    .await;
    let enter_us: u64 = out.trim().parse().ok()?;
    if enter_us == 0 {
        return None;
    }
    let up = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = up.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_secs_f64(secs).saturating_sub(Duration::from_micros(enter_us)))
}

/// Drop a death report into ~/verity/deaths — death-watch posts it to
/// Healthity's topic. Same cause on the same day overwrites itself, so
/// a restart loop can't flood the topic.
async fn report(slug: &str, body: &str) {
    let date = match tokio::process::Command::new("date")
        .arg("+%F")
        .output()
        .await
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "undated".into(),
    };
    let dir = match Config::home() {
        Ok(h) => h.join("deaths"),
        Err(_) => return,
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{date}-watchdog-{slug}.md"));
    let _ = std::fs::write(&path, body);
}

/// One watchdog pass (also `verity watchdog` by hand): start the unit
/// when down, restart it when its heartbeat went stale. Prints what it
/// did so the journal carries the resurrection.
pub async fn check() -> Result<()> {
    let unit = std::env::var("VERITY_UNIT").unwrap_or_else(|_| "verity.service".into());
    if asleep_path().exists() {
        println!("asleep flag set — staying dead");
        return Ok(());
    }
    match systemctl(&["is-enabled", &unit]).await.as_str() {
        "enabled" | "static" | "indirect" | "alias" | "generated" | "transient" => {}
        other => {
            println!("{unit} is {other} — staying dead");
            return Ok(());
        }
    }
    if systemctl(&["is-active", &unit]).await != "active" {
        println!("{unit} not active — resurrecting");
        report(
            "service-inactive",
            &format!(
                "# Death report — {unit} down\n\n\
                 The watchdog found the unit not active (no crash — those\n\
                 `Restart=always` covers itself) and started it again. If this\n\
                 was a deliberate stop, `touch ~/.local/state/verity-asleep`\n\
                 or `systemctl --user disable {unit}` tells the watchdog to\n\
                 keep its hands off.\n"
            ),
        )
        .await;
        println!("start: {}", systemctl(&["start", &unit]).await);
        return Ok(());
    }
    match last_beat() {
        Some(ts) if now().saturating_sub(ts) > STALE_AFTER.as_secs() => {
            let age = now() - ts;
            warn!(age, "heartbeat stale — restarting {unit}");
            report(
                "stale-heartbeat",
                &format!(
                    "# Death report — stale heartbeat\n\n\
                     `{unit}` was `active` but its heartbeat had gone\n\
                     {age} s without a beat (limit {} s) — the process was\n\
                     alive and dead at the same time. The watchdog restarted\n\
                     it.\n",
                    STALE_AFTER.as_secs()
                ),
            )
            .await;
            println!(
                "heartbeat {age}s stale — restart: {}",
                systemctl(&["restart", &unit]).await
            );
        }
        Some(_) => println!("alive"),
        None => {
            // No heartbeat file at all: healthy only while the unit is
            // still young — an old unit with no file never ran serve.
            match unit_uptime(&unit).await {
                Some(up) if up > STALE_AFTER => {
                    println!("no heartbeat after {}s — restarting", up.as_secs());
                    println!("restart: {}", systemctl(&["restart", &unit]).await);
                }
                _ => println!("alive (heartbeat pending)"),
            }
        }
    }
    Ok(())
}
