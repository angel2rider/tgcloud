use thiserror::Error;

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

    #[error("Invalid configuration: {0}")]
    ConfigError(String),

    #[error("Upload failed: {0}")]
    UploadFailed(String),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

pub type Result<T> = std::result::Result<T, TgCloudError>;
