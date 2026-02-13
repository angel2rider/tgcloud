use crate::storage::MongoStore;
use crate::telegram_client::TelegramClient;
use crate::bot_manager::BotManager;
use crate::models::{File, UploadEvent, UploadStatus};
use crate::errors::Result;
use tokio::sync::mpsc;
use sha2::{Sha256, Digest};
use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct TgCloudService {
    store: MongoStore,
    telegram: TelegramClient,
    bot_manager: BotManager,
    chat_id: String,
}

impl TgCloudService {
    pub async fn new(config: crate::config::Config) -> Result<Self> {
        let store = MongoStore::new(&config.mongo_uri).await?;
        let telegram = TelegramClient::new(config.telegram_api_url.clone());
        let bot_manager = BotManager::new(store.clone());

        // Auto-register bots from config
        for bot_config in config.bots {
            let bot = crate::models::Bot {
                id: None,
                bot_id: bot_config.bot_id,
                token: bot_config.token,
                upload_count: 0,
                active: true,
            };
            store.add_bot(bot).await?;
        }

        Ok(Self {
            store,
            telegram,
            bot_manager,
            chat_id: config.telegram_chat_id,
        })
    }

    pub async fn upload_file(&self, path: &str, sender: mpsc::Sender<UploadEvent>) -> Result<()> {
        let _ = sender.send(UploadEvent { status: UploadStatus::Started }).await;
        
        // 1. Select bot
        let bot = self.bot_manager.get_upload_bot().await?;
        
        // 2. Upload to Telegram
        // We clone sender to pass to callback if we implement detailed progress later
        // For now, we just wait
        let (telegram_file_id, message_id) = self.telegram.upload_file(
            &bot.token, 
            &self.chat_id, 
            path, 
            |_| {} // No-op callback for now as we don't have easy stream progress wrapper ready
        ).await?;
        
        // 3. Increment usage
        self.bot_manager.increment_usage(&bot.bot_id).await?;

        // 4. Calculate hash and size
        let file_fs = tokio::fs::File::open(path).await?;
        let metadata = file_fs.metadata().await?;
        let size = metadata.len();
        
        let mut hasher = Sha256::new();
        let mut file_for_hash = tokio::fs::File::open(path).await?; // Re-open or seek? separate handle is safer async
        let mut buffer = [0u8; 8192];
        loop {
            let n = file_for_hash.read(&mut buffer).await?;
            if n == 0 { break; }
            hasher.update(&buffer[..n]);
        }
        let hash = hex::encode(hasher.finalize()); 

        // 5. Save metadata
        let file = File {
            id: None,
            path: path.to_string(),
            bot_id: bot.bot_id.clone(),
            telegram_file_id: telegram_file_id.clone(),
            message_id: Some(message_id),
            size,
            hash,
            created_at: Utc::now(),
        };

        self.store.save_file(file).await?;

        let _ = sender.send(UploadEvent { 
            status: UploadStatus::Completed { file_id: telegram_file_id } 
        }).await;

        Ok(())
    }

    pub async fn rename_file(&self, old_path: &str, new_path: &str) -> Result<()> {
        self.store.rename_file(old_path, new_path).await
    }

    pub async fn delete_file(&self, path: &str) -> Result<()> {
        // 1. Fetch metadata
        let file_opt: Option<File> = self.store.get_file_by_path(path).await?;
        let file = file_opt.ok_or_else(|| crate::errors::TgCloudError::FileNotFound(path.to_string()))?;

        // 2. Get bot token
        let token = self.bot_manager.get_bot_token(&file.bot_id).await?;

        // 3. Delete from Telegram
        if let Some(msg_id) = file.message_id {
            self.telegram.delete_message(&token, &self.chat_id, msg_id).await?;
        } else {
            // Legacy file without message_id. We cannot delete it from Telegram (we don't know the ID).
            // We proceed to delete from Mongo to allow cleanup.
            // In a real app, maybe log a warning here.
        }

        // 4. Delete from Mongo (only if TG delete succeeded or was skipped for legacy)
        self.store.delete_file(path).await?;

        Ok(())
    }

    pub async fn download_file(&self, path: &str, output_path: &str, sender: mpsc::Sender<crate::models::DownloadEvent>) -> Result<()> {
        use crate::models::{DownloadEvent, DownloadStatus};

        let file_opt: Option<File> = self.store.get_file_by_path(path).await?;
        let file = file_opt.ok_or_else(|| crate::errors::TgCloudError::FileNotFound(path.to_string()))?;

        let _ = sender.send(DownloadEvent { status: DownloadStatus::Started { total_size: file.size } }).await;

        let token = self.bot_manager.get_bot_token(&file.bot_id).await
            .map_err(|e| {
                let _ = sender.try_send(DownloadEvent { status: DownloadStatus::Failed { error: e.to_string() } });
                e
            })?;

        let url = self.telegram.get_download_url(&token, &file.telegram_file_id).await
             .map_err(|e| {
                let _ = sender.try_send(DownloadEvent { status: DownloadStatus::Failed { error: e.to_string() } });
                e
            })?;
        
        let mut response = self.telegram.download_file(&url).await
             .map_err(|e| {
                let _ = sender.try_send(DownloadEvent { status: DownloadStatus::Failed { error: e.to_string() } });
                e
            })?;

        let mut out_file = tokio::fs::File::create(output_path).await?;
        
        let mut downloaded = 0;
        while let Some(chunk) = response.chunk().await.map_err(crate::errors::TgCloudError::TelegramError)? {
             out_file.write_all(&chunk).await?;
             downloaded += chunk.len() as u64;
             let _ = sender.try_send(DownloadEvent { status: DownloadStatus::Progress { downloaded, total: file.size } });
        }

        let _ = sender.send(DownloadEvent { status: DownloadStatus::Completed { path: output_path.to_string() } }).await;
        Ok(())
    }

    pub async fn list_files(&self, prefix: &str) -> Result<Vec<File>> {
        self.store.list_files(prefix).await
    }


}
