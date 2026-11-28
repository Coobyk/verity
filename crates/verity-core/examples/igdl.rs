//! Send a link to an Instagram-downloader bot and save the media it
//! replies with. Tries each known bot in order until one delivers.
//!
//! `cargo run -p verity-core --example igdl -- <url> [outdir]`

use std::path::PathBuf;
use verity_core::config::Config;
use verity_core::mtproto::Tg;

const BOTS: &[&str] = &[
    "vinstebot",
    "SaveAsBot",
    "instanavy_bot",
    "instagram_load_bot",
    "instasavegrambot",
    "instagram_dload_bot",
    "download_it_bot",
    "InstagramRobot",
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let link = args.next().expect("instagram url");
    let outdir = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/ig".into()));

    let cfg = Config::load()?;
    let (api_id, api_hash) = cfg.api_credentials()?;
    let tg = Tg::connect(api_id, api_hash, &Config::session_path()?).await?;

    for bot in BOTS {
        let Some(peer) = tg.resolve_username_peer(bot).await? else {
            println!("{bot}: no such user, skipping");
            continue;
        };
        match tg.fetch_media_reply(peer, &link, &outdir, 75).await {
            Ok(paths) => {
                println!("{bot}: saved {} file(s)", paths.len());
                for p in paths {
                    println!("{}", p.display());
                }
                return Ok(());
            }
            Err(e) => println!("{bot}: {e}"),
        }
    }
    Err("no bot delivered".into())
}
