use crate::errors::{Result, TgCloudError};
use crate::models::Bot;
use crate::storage::MongoStore;

pub struct BotManager {
    store: MongoStore,
}

impl BotManager {
    pub fn new(store: MongoStore) -> Self {
        Self { store }
    }

    pub async fn get_upload_bot(&self) -> Result<Bot> {
        let mut bots: Vec<Bot> = self.store.get_active_bots().await?;
        if bots.is_empty() {
            return Err(TgCloudError::BotManagerError(
                "No active bots found".to_string(),
            ));
        }

        // Simple strategy: pick the one with lowest upload count
        bots.sort_by_key(|b| b.upload_count);

        Ok(bots[0].clone())
    }

    pub async fn get_bot_token(&self, bot_id: &str) -> Result<String> {
        // In a real app we might cache this map in memory
        let bots: Vec<Bot> = self.store.get_active_bots().await?;
        let bot = bots
            .into_iter()
            .find(|b| b.bot_id == bot_id)
            .ok_or_else(|| TgCloudError::BotManagerError(format!("Bot {} not found", bot_id)))?;
        Ok(bot.token)
    }

    pub async fn increment_usage(&self, bot_id: &str) -> Result<()> {
        self.store.increment_bot_usage(bot_id).await
    }
}
