//! Fetch the sticker set a message's sticker belongs to and download
//! every sticker in it.
//!
//! `cargo run -p verity-core --example stickers -- <peer> <msg_id> [dir]`

use std::path::PathBuf;
use verity_core::config::Config;
use verity_core::mtproto::Tg;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let who = args.next().expect("peer (username or id)");
    let msg_id: i32 = args.next().expect("msg id").parse()?;
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/stickers".into()));

    let cfg = Config::load()?;
    let (api_id, api_hash) = cfg.api_credentials()?;
    let tg = Tg::connect(api_id, api_hash, &Config::session_path()?).await?;
    let peer = tg.peer(&who).await?;
    let pack = tg.sticker_pack(peer, msg_id, &dir).await?;
    println!(
        "{} (t.me/addstickers/{}) — {} stickers",
        pack.title, pack.short_name, pack.count
    );
    for s in &pack.stickers {
        println!("  {} {} {}", s.emoji, s.mime, s.path.display());
    }
    Ok(())
}
