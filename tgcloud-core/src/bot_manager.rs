use crate::errors::{Result, TgCloudError};
use crate::models::Bot;
use crate::storage::MongoStore;
use std::collections::HashMap;
use tokio::sync::RwLock;

/// Manages bot selection, token resolution, and usage tracking.
///
/// Maintains an in-memory cache of bot tokens to avoid repeated DB lookups
/// during concurrent chunk operations.
pub struct BotManager {
    store: MongoStore,
    /// Cached mapping of bot_id -> token. Populated on first access and
    /// refreshed when `get_all_active_bots` is called.
    token_cache: RwLock<HashMap<String, String>>,
}

impl BotManager {
    pub fn new(store: MongoStore) -> Self {
        Self {
            store,
            token_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Refresh the in-memory token cache from the database.
    async fn refresh_cache(&self) -> Result<Vec<Bot>> {
        let bots = self.store.get_active_bots().await?;
        let mut cache = self.token_cache.write().await;
        cache.clear();
        for bot in &bots {
            cache.insert(bot.bot_id.clone(), bot.token.clone());
        }
        Ok(bots)
    }

    /// Pick the bot with the lowest upload count (for single-bot, small-file uploads).
    pub async fn get_upload_bot(&self) -> Result<Bot> {
        let mut bots = self.refresh_cache().await?;
        if bots.is_empty() {
            return Err(TgCloudError::BotManagerError(
                "No active bots found".to_string(),
            ));
        }

        // Simple strategy: pick the one with lowest upload count.
        bots.sort_by_key(|b| b.upload_count);
        Ok(bots[0].clone())
    }

    /// Return all active bots, refreshing the token cache.
    pub async fn get_all_active_bots(&self) -> Result<Vec<Bot>> {
        let bots = self.refresh_cache().await?;
        if bots.is_empty() {
            return Err(TgCloudError::BotManagerError(
                "No active bots found".to_string(),
            ));
        }
        Ok(bots)
    }

    /// Resolve a bot token by bot_id, using the in-memory cache first.
    pub async fn get_bot_token(&self, bot_id: &str) -> Result<String> {
        // Try cache first.
        {
            let cache = self.token_cache.read().await;
            if let Some(token) = cache.get(bot_id) {
                return Ok(token.clone());
            }
        }

        // Cache miss — refresh from DB and try again.
        self.refresh_cache().await?;
        let cache = self.token_cache.read().await;
        cache
            .get(bot_id)
            .cloned()
            .ok_or_else(|| TgCloudError::BotManagerError(format!("Bot {} not found", bot_id)))
    }

    /// Build a map of bot_id -> token for a set of bot IDs.
    /// Uses the cache, refreshing on miss.
    pub async fn get_token_map(&self, bot_ids: &[String]) -> Result<HashMap<String, String>> {
        // Ensure cache is populated.
        {
            let cache = self.token_cache.read().await;
            let all_found = bot_ids.iter().all(|id| cache.contains_key(id));
            if all_found {
                let map: HashMap<String, String> = bot_ids
                    .iter()
                    .filter_map(|id| cache.get(id).map(|t| (id.clone(), t.clone())))
                    .collect();
                return Ok(map);
            }
        }

        // Some IDs missing — refresh.
        self.refresh_cache().await?;
        let cache = self.token_cache.read().await;
        let mut map = HashMap::with_capacity(bot_ids.len());
        for id in bot_ids {
            let token = cache
                .get(id)
                .ok_or_else(|| TgCloudError::BotManagerError(format!("Bot {} not found", id)))?;
            map.insert(id.clone(), token.clone());
        }
        Ok(map)
    }

    pub async fn increment_usage(&self, bot_id: &str) -> Result<()> {
        self.store.increment_bot_usage(bot_id).await
    }
}
