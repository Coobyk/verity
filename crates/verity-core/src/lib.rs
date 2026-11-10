//! Verity core: named teammate bots that live in a Telegram forum supergroup.
//!
//! - [`botapi`]: thin Telegram Bot API client (rich messages, drafts, forum topics)
//! - [`mtproto`]: user-side MTProto client via grammers (login, history, topics)
//! - [`store`]: SQLite brain — bot records, memory, thread state
//! - [`harness`]: provider seam; agents run behind `Harness`
//! - [`bridge`]: the quiet-bot loop routing topic messages to bots

pub mod backup;
pub mod botapi;
pub mod bridge;
pub mod config;
pub mod error;
pub mod harness;
pub mod mtproto;
pub mod rich;
pub mod store;

pub use error::{Error, Result};
