//! `verity setup` — interactive first-run wizard. Walks API credentials,
//! Telegram login, harness choice, and bot creation end to end. Every
//! step is skip-aware: re-running it on a configured machine only fills
//! in what's missing.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use verity_core::botapi::BotApi;
use verity_core::config::Config;
use verity_core::mtproto::Tg;
use verity_core::store::{Bot, Store};

use crate::prompt;

/// Prompt that treats stdin EOF as cancel rather than an endless stream
/// of empty answers.
fn ask(label: &str) -> Result<String> {
    prompt(label)
}

fn ask_required(label: &str) -> Result<String> {
    loop {
        let s = ask(label)?;
        if !s.is_empty() {
            return Ok(s);
        }
        println!("  (required)");
    }
}

fn ask_yn(label: &str, default: bool) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        let s = ask(&format!("{label} [{hint}]"))?;
        match s.to_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("  answer y or n"),
        }
    }
}

/// Network calls in grammers can wait forever (wrong clock, filtered
/// network); cap them and say something useful.
async fn net<T>(what: &str, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(Duration::from_secs(20), fut).await {
        Ok(r) => r,
        Err(_) => bail!(
            "{what}: Telegram did not answer within 20s — check the network \
             and that your system clock is correct (clock skew breaks MTProto)"
        ),
    }
}

/// Is `bin` discoverable on PATH? Used to warn early when the chosen
/// harness isn't installed yet.
fn on_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.BAT;.CMD".into())
            .split(';')
            .map(|e| e.to_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&paths).any(|dir| {
        let cand = dir.join(bin);
        if cand.is_file() {
            return true;
        }
        exts.iter()
            .any(|ext| !ext.is_empty() && dir.join(format!("{bin}{ext}")).is_file())
    })
}

/// True when the machine still needs the wizard: no API credentials or
/// no registered bots. Drives bare-`verity` behavior.
pub fn needs_setup() -> bool {
    let cfg = Config::load().unwrap_or_default();
    if cfg.api_credentials().is_err() {
        return true;
    }
    let bots = Config::db_path()
        .and_then(|p| Store::open(&p))
        .and_then(|s| s.list_bots())
        .unwrap_or_default();
    bots.is_empty()
}

pub async fn run() -> Result<()> {
    println!("verity setup — end-to-end wizard. Ctrl-C any time to bail out.\n");

    // ── Step 1: API credentials ─────────────────────────────────────
    let mut cfg = Config::load()?;
    let have_creds = cfg.api_id.is_some() && cfg.api_hash.is_some();
    let change = have_creds && ask_yn("API credentials already configured — change them?", false)?;
    if !have_creds || change {
        println!("── step 1/4: Telegram API credentials");
        println!("   create them at https://my.telegram.org → API development tools");
        cfg.api_id = Some(loop {
            let s = ask_required("api_id")?;
            match s.trim().parse::<i32>() {
                Ok(v) if v > 0 => break v,
                _ => println!("  api_id must be a positive number"),
            }
        });
        cfg.api_hash = Some(ask_required("api_hash")?);
        cfg.save()?;
        println!("   saved\n");
    } else {
        println!("✓ API credentials configured\n");
    }

    // ── Step 2: Telegram login ──────────────────────────────────────
    let (api_id, api_hash) = cfg.api_credentials()?;
    let tg = Tg::connect(api_id, api_hash, &Config::session_path()?).await?;
    let authed = net("auth check", async {
        tg.is_authorized()
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    })
    .await?;
    if authed {
        println!("✓ already logged in to Telegram\n");
    } else {
        println!("── step 2/4: log in as yourself");
        println!("   verity bots live in DMs with your own account");
        let phone = ask_required("phone (+country code, e.g. +15551234567)")?;
        tg.sign_in(&phone, |what| {
            crate::prompt(what).map_err(|e| verity_core::Error::Other(e.to_string()))
        })
        .await?;
        tg.save_session(&Config::session_path()?)?;
        println!("   logged in\n");
    }

    // ── Step 3: harness ─────────────────────────────────────────────
    println!("── step 3/4: which agent harness should bots use?");
    println!("   1) pi    — the pi agent CLI (default)");
    println!("   2) echo  — echo test harness, no agent (plumbing check)");
    println!("   3) acp   — any ACP-speaking agent CLI (claude-code-acp, codex acp, …)");
    let provider = loop {
        match ask("choose 1-3 [1]")?.as_str() {
            "" | "1" | "pi" => {
                let default_bin = cfg.pi_bin.clone().unwrap_or_else(|| "pi".into());
                let bin = {
                    let s = ask(&format!("pi binary [{default_bin}]"))?;
                    if s.is_empty() {
                        default_bin
                    } else {
                        s
                    }
                };
                cfg.pi_bin = Some(bin.clone());
                cfg.save()?;
                if !on_path(&bin) {
                    println!("   warning: `{bin}` is not on PATH — install it before serving");
                }
                break "pi".to_string();
            }
            "2" | "echo" => break "echo".to_string(),
            "3" | "acp" => {
                let spec = ask_required("acp command (e.g. claude-code-acp or `pi --acp`)")?;
                let bin = spec.split_whitespace().next().unwrap_or("");
                if !bin.is_empty() && !on_path(bin) {
                    println!("   warning: `{bin}` is not on PATH — install it before serving");
                }
                break format!("acp:{spec}");
            }
            _ => println!("  pick 1, 2 or 3"),
        }
    };
    println!("   provider: {provider}\n");

    // ── Step 4: bots ────────────────────────────────────────────────
    println!("── step 4/4: bots");
    let store = Store::open(&Config::db_path()?)?;
    let existing = store.list_bots()?;
    if !existing.is_empty() {
        println!("   already registered:");
        for b in &existing {
            let user = b
                .username
                .as_deref()
                .map(|u| format!("@{u}"))
                .unwrap_or_else(|| "-".into());
            println!("   {:<16} {:<24} provider={}", b.name, user, b.provider);
        }
    }

    let mut created: Vec<Bot> = Vec::new();
    let want_default = existing.is_empty();
    while ask_yn(
        if created.is_empty() && existing.is_empty() {
            "create your first bot?"
        } else {
            "add another bot?"
        },
        want_default,
    )? {
        let mode_create = ask_yn(
            "create a new bot via @BotFather? (no = register an existing token)",
            true,
        )?;
        let name = ask_required("bot name (roster label, e.g. rey)")?;
        let instructions = ask("personality/instructions (blank for none)")?;
        let model = {
            let s = ask("model override (blank for harness default)")?;
            (!s.is_empty()).then_some(s)
        };
        let greeting = {
            let s = ask("greeting — first message when the DM opens (blank for none)")?;
            (!s.is_empty()).then_some(s)
        };

        let bot = if mode_create {
            let suggested = suggest_username(&name);
            let token = loop {
                let s = ask(&format!("bot username (must end in bot) [{suggested}]"))?;
                let username = if s.is_empty() { suggested.clone() } else { s };
                if !username.ends_with("bot") {
                    println!("  bot usernames must end in \"bot\"");
                    continue;
                }
                println!("   asking @BotFather…");
                match net("create bot", async {
                    tg.create_bot(&name, &username)
                        .await
                        .map_err(|e| anyhow::anyhow!(e.to_string()))
                })
                .await
                {
                    Ok(t) => break t,
                    Err(e) => {
                        println!("   BotFather refused: {e}");
                        if !ask_yn("try a different username?", true)? {
                            bail!("bot creation cancelled");
                        }
                    }
                }
            };
            let me = net("getMe", async {
                BotApi::new(&token)
                    .get_me()
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
            })
            .await?;
            Bot {
                name: name.clone(),
                token,
                username: me.username,
                instructions: instructions.clone(),
                provider: provider.clone(),
                model: model.clone(),
                greeting: greeting.clone(),
            }
        } else {
            let token = ask_required("bot token (from @BotFather)")?;
            let me = net("getMe", async {
                BotApi::new(&token)
                    .get_me()
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
            })
            .await
            .context("token rejected by Telegram — check it")?;
            Bot {
                name: name.clone(),
                token,
                username: me.username,
                instructions: instructions.clone(),
                provider: provider.clone(),
                model: model.clone(),
                greeting: greeting.clone(),
            }
        };

        store.add_bot(&bot)?;
        println!(
            "   registered {} (@{})",
            bot.name,
            bot.username.as_deref().unwrap_or("?")
        );
        if ask_yn("generate a profile picture for it?", true)? {
            let seed = verity_core::pfp::name_seed(&bot.name);
            match crate::set_pfp(&bot, seed, None).await {
                Ok(()) => {}
                Err(e) => println!("   pfp failed (skipping): {e}"),
            }
        }
        created.push(bot);
    }

    // ── Done ────────────────────────────────────────────────────────
    println!("\nsetup complete.");
    if !created.is_empty() || !existing.is_empty() {
        println!("next steps:");
        println!(
            "  1. start the bridge:  verity supervise   (or `verity serve` in the foreground)"
        );
        for b in store.list_bots()? {
            if let Some(u) = b.username {
                println!("  2. in Telegram, open a DM with @{u} and press /start");
                break;
            }
        }
    }
    Ok(())
}

/// Bot usernames must be unique globally; suffix with random digits so
/// the suggestion usually passes BotFather on the first try.
fn suggest_username(name: &str) -> String {
    let base: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(20)
        .collect::<String>()
        .to_lowercase();
    let base = if base.is_empty() {
        "verity".into()
    } else {
        base
    };
    let n = (crate::rand_u64() % 9000) + 1000;
    format!("{base}{n}bot")
}
