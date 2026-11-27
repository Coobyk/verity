//! `vsk1.` setup keys: one short string carrying everything a fresh
//! machine needs to pull the whole install back down — the bot token
//! that can fetch the archive, the archive's file_id, and its AES
//! password.
//!
//! `verity getsetupkey` issues one (packs `~/verity` into an
//! AES-encrypted 7z, uploads it to the owner's DM via the lead bot);
//! `verity --setupkey <key>` consumes it: getFile + download over the
//! plain Bot API (no login, no config), decrypt, unpack, re-enable
//! units. The key alone is a full credential — it ends up holding
//! `tg.session` — treat it like a password.

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::botapi::BotApi;
use crate::{backup, Error, Result};

/// Versioned prefix so a pasted key is recognizable and future formats
/// can coexist (`vsk2.` etc.).
const PREFIX: &str = "vsk1.";

#[derive(Serialize, Deserialize)]
struct Key {
    /// Bot token used to getFile + download the archive; it must be the
    /// bot that sent the document — file_ids are per-bot.
    t: String,
    /// file_id of the encrypted archive in the owner's DM.
    f: String,
    /// The archive's 7z AES-256 password.
    p: String,
}

/// Pack an encrypted setup archive, upload it to `owner`'s DM with
/// `api`'s bot (`token` is that same bot's token — it travels in the
/// key), and return the `vsk1.` key that unlocks it.
pub async fn issue(api: &BotApi, token: &str, owner: i64) -> Result<String> {
    let password = new_password();
    let (file_id, _name) = backup::upload_setup(api, owner, &password).await?;
    let json = serde_json::to_string(&Key {
        t: token.to_string(),
        f: file_id,
        p: password,
    })
    .map_err(|e| Error::Other(e.to_string()))?;
    Ok(format!("{PREFIX}{}", b64().encode(json)))
}

/// Apply a `vsk1.` key: download the archive it points at and unpack
/// it into `~/verity`. Needs nothing else on the machine. Returns the
/// names restored — feed them to [`backup::post_restore`].
pub async fn apply(key: &str) -> Result<Vec<String>> {
    let k = decode(key)?;
    let api = BotApi::new(&k.t);
    backup::restore_setup(&api, &k.f, &k.p).await
}

fn decode(key: &str) -> Result<Key> {
    let bad = || Error::Config("malformed setup key — expected `vsk1.…`".into());
    let body = key.trim().strip_prefix(PREFIX).ok_or_else(bad)?;
    let json = b64().decode(body).map_err(|_| bad())?;
    serde_json::from_slice(&json).map_err(|_| bad())
}

fn b64() -> impl Engine {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// 192 bits of /dev/urandom, base64url'd — the archive password.
fn new_password() -> String {
    use std::io::Read;
    let mut b = [0u8; 24];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        // /dev/urandom doesn't realistically fail on linux, but a weak
        // fallback beats panicking or shipping a constant.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0x9e3779b97f4a7c15);
        for (i, v) in b.iter_mut().enumerate() {
            *v = (n >> ((i % 16) * 8)) as u8 ^ (std::process::id() as u8).wrapping_mul(i as u8 + 1);
        }
    }
    b64().encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Key encode → decode round-trips every field; junk and foreign
    /// strings are refused.
    #[test]
    fn key_roundtrip() {
        let k = Key {
            t: "123:abc".into(),
            f: "file_id".into(),
            p: "pw".into(),
        };
        let s = format!(
            "{PREFIX}{}",
            b64().encode(serde_json::to_string(&k).unwrap())
        );
        let k2 = decode(&format!("  {s}\n")).unwrap();
        assert_eq!(
            (k2.t, k2.f, k2.p),
            ("123:abc".into(), "file_id".into(), "pw".into())
        );

        assert!(decode("not-a-key").is_err());
        assert!(decode("vsk1.!!!").is_err());
    }
}
