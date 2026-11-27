//! Watch `~/verity/deaths` for new `*.md` reports and post them into
//! the owner's DM with a bot as file documents. Filenames already sent
//! are tracked in `~/.local/state/verity-deaths-sent` so restarts never
//! repost. `verity death-watch` — verity-death-watch.service,
//! `Restart=always`; one pass every 10 s.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use crate::botapi::BotApi;
use crate::config::Config;
use crate::Result;

fn state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state")
}

/// Watch the deaths dir forever, sending each new `*.md` to `owner` as
/// a document through `api`. A failed send is retried on the next pass.
pub async fn watch(api: &BotApi, owner: i64) -> Result<()> {
    let deaths = Config::home()?.join("deaths");
    let state = state_dir().join("verity-deaths-sent");
    std::fs::create_dir_all(state_dir())?;
    // Union the legacy Healthity-era ledger so already-posted reports
    // aren't sent again after the rename.
    let mut sent: BTreeSet<String> = [state_dir().join("verity-healthity-sent"), state.clone()]
        .iter()
        .flat_map(|p| {
            std::fs::read_to_string(p)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    loop {
        let mut names: Vec<PathBuf> = std::fs::read_dir(&deaths)
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        names.sort();
        for path in names {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".md") || sent.contains(name) {
                continue;
            }
            if api.send_document(owner, &path, None).await.is_ok() {
                sent.insert(name.to_string());
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&state)
                {
                    let _ = writeln!(f, "{name}");
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
