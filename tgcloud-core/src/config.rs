use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub mongo_uri: String,
    pub telegram_api_url: String,
    pub telegram_chat_id: String,
    pub bots: Vec<BotConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    pub bot_id: String,
    pub token: String,
}
