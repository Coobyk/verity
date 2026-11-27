//! Verity core: named teammate bots you chat with in 1:1 Telegram DMs.
//!
//! - [`botapi`]: thin Telegram Bot API client (rich messages, drafts)
//! - [`mtproto`]: user-side MTProto client via grammers (login, DM history)
//! - [`store`]: SQLite brain — bot records, memory, turn state
//! - [`harness`]: provider seam; agents run behind `Harness`
//! - [`bridge`]: the quiet-bot loop routing the owner's DMs to bots
//! - [`api`]: bearer-gated JSON API for the mobile app (`api_bind`/`api_token`)

pub mod api;
pub mod backup;
pub mod botapi;
pub mod bridge;
pub mod config;
pub mod deathwatch;
pub mod error;
pub mod harness;
pub mod liveness;
pub mod mtproto;
pub mod rich;
pub mod selfupdate;
pub mod setupkey;
pub mod store;
pub mod supervisor;

pub use verity_pfp as pfp;

pub use error::{Error, Result};
