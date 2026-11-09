#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("bot api error {code}: {description}")]
    BotApi { code: i64, description: String },
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] sqlite::Error),
    #[error("config: {0}")]
    Config(String),
    #[error("mtproto: {0}")]
    Mtproto(String),
    #[error("harness: {0}")]
    Harness(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
