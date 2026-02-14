use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("BOTS_JSON environment variable not set")]
    MissingBotsJson,

    #[error("Invalid BOTS_JSON: {0}")]
    InvalidBotsJson(String),

    #[error("{0} must be set")]
    MissingEnvVar(String),

    #[error("Neither BOTS_JSON nor BOT_ID/BOT_TOKEN provided")]
    NoValidBotConfig,

    #[error("Configuration error: {0}")]
    General(String),
}

#[derive(Error, Debug)]
pub enum TgCloudError {
    #[error("MongoDB error: {0}")]
    MongoError(#[from] mongodb::error::Error),

    #[error("Telegram API error: {0}")]
    TelegramError(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Bot manager error: {0}")]
    BotManagerError(String),

    #[error("File not found: {0}")]
    FileNotFound(String),

    #[error("Configuration error: {0}")]
    ConfigError(#[from] ConfigError),

    #[error("Upload failed: {0}")]
    UploadFailed(String),

    #[error("Download failed: {0}")]
    DownloadFailed(String),

    #[error("Delete failed: {0}")]
    DeleteFailed(String),

    #[error("Integrity error: {0}")]
    IntegrityFailed(String),

    #[error("Rate limited: {0}")]
    RateLimited(String),

    #[error("Retry exhausted after {attempts} attempts: {last_error}")]
    RetryExhausted { attempts: u32, last_error: String },

    #[error("Unknown error: {0}")]
    Unknown(String),
}

pub type Result<T> = std::result::Result<T, TgCloudError>;
