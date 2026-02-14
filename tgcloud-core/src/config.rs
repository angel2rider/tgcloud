use crate::errors::ConfigError;
use serde::{Deserialize, Serialize};
use std::env;

/// Default maximum number of concurrent chunk operations across all bots.
pub const DEFAULT_MAX_GLOBAL_CONCURRENCY: usize = 12;
/// Default maximum number of concurrent chunk operations per individual bot.
pub const DEFAULT_MAX_PER_BOT_CONCURRENCY: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub mongo_uri: String,
    pub telegram_api_url: String,
    pub telegram_chat_id: String,
    pub bot_id: String,
    pub bot_token: String,
    /// Maximum number of concurrent chunk operations.
    pub max_concurrency: usize,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| ConfigError::General("Could not resolve config directory".into()))?
            .join("tgcloud");

        if !config_dir.exists() {
            std::fs::create_dir_all(&config_dir)
                .map_err(|e| ConfigError::General(format!("Failed to create config directory: {}", e)))?;
        }

        let config_path = config_dir.join(".env");
        if !config_path.exists() {
            eprintln!(
                "TGCloud config not found at {}. Please create ~/.config/tgcloud/.env (or equivalent on your OS)",
                config_path.display()
            );
            std::process::exit(1);
        }

        dotenv::from_path(&config_path).ok();

        let mongo_uri = env::var("MONGO_URI")
            .map_err(|_| ConfigError::MissingEnvVar("MONGO_URI".into()))?;
        if mongo_uri.trim().is_empty() {
            return Err(ConfigError::MissingEnvVar("MONGO_URI".into()));
        }

        let telegram_api_url =
            env::var("TELEGRAM_API_URL").unwrap_or_else(|_| "http://localhost:8081".to_string());

        let telegram_chat_id = env::var("TELEGRAM_CHAT_ID")
            .map_err(|_| ConfigError::MissingEnvVar("TELEGRAM_CHAT_ID".into()))?;
        if telegram_chat_id.trim().is_empty() {
            return Err(ConfigError::MissingEnvVar("TELEGRAM_CHAT_ID".into()));
        }

        let bot_id = env::var("BOT_ID").map_err(|_| ConfigError::MissingEnvVar("BOT_ID".into()))?;
        let bot_id = bot_id.trim();
        if bot_id.is_empty() {
            return Err(ConfigError::MissingEnvVar("BOT_ID".into()));
        }

        let bot_token =
            env::var("BOT_TOKEN").map_err(|_| ConfigError::MissingEnvVar("BOT_TOKEN".into()))?;
        let bot_token = bot_token.trim();
        if bot_token.is_empty() {
            return Err(ConfigError::MissingEnvVar("BOT_TOKEN".into()));
        }

        Ok(Self {
            mongo_uri,
            telegram_api_url,
            telegram_chat_id,
            bot_id: bot_id.to_string(),
            bot_token: bot_token.to_string(),
            max_concurrency: DEFAULT_MAX_GLOBAL_CONCURRENCY,
        })
    }
}

