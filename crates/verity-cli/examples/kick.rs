use verity_core::config::Config;
use verity_core::mtproto::Tg;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let username = std::env::args().nth(1).expect("usage: kick <bot-username>");
    let cfg = Config::load()?;
    let (api_id, api_hash) = cfg.api_credentials()?;
    let tg = Tg::connect(api_id, api_hash, &Config::session_path()?).await?;
    let peer = tg.peer(&cfg.group_id.unwrap().to_string()).await?;
    for m in tg.list_bot_members(peer).await? {
        println!("member: {} {:?}", m.user_id, m.username);
        if m.username.as_deref() == Some(username.as_str()) {
            tg.kick_member(peer, m.input).await?;
            println!("kicked {username}");
            return Ok(());
        }
    }
    println!("{username} is not a member");
    Ok(())
}
