use serde::{Deserialize, Serialize};

/// Default maximum number of concurrent chunk operations across all bots.
pub const DEFAULT_MAX_GLOBAL_CONCURRENCY: usize = 12;
/// Default maximum number of concurrent chunk operations per individual bot.
pub const DEFAULT_MAX_PER_BOT_CONCURRENCY: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub mongo_uri: String,
    pub telegram_api_url: String,
    pub telegram_chat_id: String,
    pub bots: Vec<BotConfig>,
    /// Maximum number of concurrent chunk operations across all bots.
    #[serde(default = "default_global_concurrency")]
    pub max_global_concurrency: usize,
    /// Maximum number of concurrent chunk operations per individual bot.
    #[serde(default = "default_per_bot_concurrency")]
    pub max_per_bot_concurrency: usize,
}

fn default_global_concurrency() -> usize {
    DEFAULT_MAX_GLOBAL_CONCURRENCY
}

fn default_per_bot_concurrency() -> usize {
    DEFAULT_MAX_PER_BOT_CONCURRENCY
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    pub bot_id: String,
    pub token: String,
}
