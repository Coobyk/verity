//! One-shot cleanup: delete orphaned "⏳ …" progress messages from every
//! bot's DM with the owner. Dry-run by default; `--yes` deletes. Run
//! with `cargo run --release --example sweep_progress -- --yes`.

use anyhow::Result;
use verity_core::config::Config;
use verity_core::mtproto::Tg;
use verity_core::store::Store;

#[tokio::main]
async fn main() -> Result<()> {
    let yes = std::env::args().any(|a| a == "--yes");
    let cfg = Config::load()?;
    let (api_id, api_hash) = cfg.api_credentials()?;
    let tg = Tg::connect(api_id, api_hash, &Config::session_path()?).await?;
    if !tg.is_authorized().await? {
        anyhow::bail!("not logged in — run `verity login --phone <number>`");
    }

    let store = Store::open(&Config::db_path()?)?;
    for bot in store.list_bots()? {
        let uid = match verity_core::botapi::BotApi::new(&bot.token).get_me().await {
            Ok(me) => me.id,
            Err(e) => {
                eprintln!("{}: getMe failed ({e}), skipping", bot.name);
                continue;
            }
        };
        let Ok(peer) = tg.peer_for_user(uid).await else {
            eprintln!("{}: no DM, skipping", bot.name);
            continue;
        };
        let history = tg.dm_history(peer.clone(), 200).await?;
        let hits: Vec<_> = history
            .iter()
            .filter(|m| {
                m.text.starts_with('⏳')
                    && (m.sender == bot.name || Some(m.sender.as_str()) == bot.username.as_deref())
            })
            .collect();
        if hits.is_empty() {
            println!("{}: nothing to sweep", bot.name);
            continue;
        }
        for m in &hits {
            println!("{}: #{} {} — {}", bot.name, m.id, m.sender, m.text);
        }
        if yes {
            let ids: Vec<i32> = hits.iter().map(|m| m.id).collect();
            let n = tg.delete_any_messages(peer, &ids).await?;
            println!("{}: deleted {n} ⏳ message(s)", bot.name);
        }
    }
    Ok(())
}
