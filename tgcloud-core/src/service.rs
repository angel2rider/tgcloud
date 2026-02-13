use crate::storage::MongoStore;
use crate::telegram_client::TelegramClient;
use crate::bot_manager::BotManager;
use crate::models::{File, UploadEvent, UploadStatus};
use crate::errors::Result;
use tokio::sync::mpsc;
use sha2::{Sha256, Digest};
use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncSeekExt};

const MAX_CHUNK_SIZE: u64 = 2000 * 1024 * 1024; // 2000 MiB (Telegram's limit is exactly 2000MB)

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
        
        // 1. Calculate hash and total size
        let file_fs = tokio::fs::File::open(path).await?;
        let metadata = file_fs.metadata().await?;
        let total_size = metadata.len();
        
        let _ = sender.send(UploadEvent { status: UploadStatus::Hashing }).await;

        let mut hasher = Sha256::new();
        let mut file_for_hash = tokio::fs::File::open(path).await?;
        let mut buffer = [0u8; 8192];
        loop {
            let n = file_for_hash.read(&mut buffer).await?;
            if n == 0 { break; }
            hasher.update(&buffer[..n]);
        }
        let hash = hex::encode(hasher.finalize());

        // 2. Select bot
        let bot = self.bot_manager.get_upload_bot().await?;
        
        let chunked = total_size > MAX_CHUNK_SIZE;
        let total_parts: u32 = if chunked {
            ((total_size + MAX_CHUNK_SIZE - 1) / MAX_CHUNK_SIZE) as u32
        } else {
            1
        };
        let mut parts = Vec::new();
        let mut bytes_sent = 0;

        if !chunked {
            // Small file flow (1 part)
            let _ = sender.send(UploadEvent {
                status: UploadStatus::ChunkStarted { part_number: 1, total_parts: 1, chunk_size: total_size }
            }).await;
            let (tg_id, msg_id) = self.telegram.upload_file(&bot.token, &self.chat_id, path, |_| {}).await?;
            parts.push(crate::models::FilePart {
                part_number: 1,
                telegram_file_id: tg_id.clone(),
                message_id: msg_id,
                size: total_size,
            });
            bytes_sent = total_size;
            let _ = sender.send(UploadEvent { 
                status: UploadStatus::Progress { sent: bytes_sent, total: total_size }
            }).await;
            let _ = sender.send(UploadEvent {
                status: UploadStatus::ChunkCompleted { part_number: 1, total_parts: 1 }
            }).await;
        } else {
            // Chunked upload flow
            let mut file = tokio::fs::File::open(path).await?;
            let mut part_num = 1;
            let mut remaining = total_size;
            
            while remaining > 0 {
                let current_chunk_size = std::cmp::min(remaining, MAX_CHUNK_SIZE);

                let _ = sender.send(UploadEvent {
                    status: UploadStatus::ChunkStarted { part_number: part_num, total_parts, chunk_size: current_chunk_size }
                }).await;

                let part_reader = file.take(current_chunk_size);
                
                let file_name = format!("{}.part{}", 
                    std::path::Path::new(path).file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".to_string()),
                    part_num
                );

                let upload_res = self.telegram.upload_part(
                    &bot.token, 
                    &self.chat_id, 
                    file_name,
                    part_reader
                ).await;

                match upload_res {
                    Ok((tg_id, msg_id)) => {
                        parts.push(crate::models::FilePart {
                            part_number: part_num,
                            telegram_file_id: tg_id,
                            message_id: msg_id,
                            size: current_chunk_size,
                        });
                        bytes_sent += current_chunk_size;
                        let _ = sender.send(UploadEvent { 
                            status: UploadStatus::Progress { sent: bytes_sent, total: total_size }
                        }).await;
                        let _ = sender.send(UploadEvent {
                            status: UploadStatus::ChunkCompleted { part_number: part_num, total_parts }
                        }).await;
                        
                        remaining -= current_chunk_size;
                        part_num += 1;
                        
                        file = tokio::fs::File::open(path).await?;
                        file.seek(std::io::SeekFrom::Start(bytes_sent)).await?;
                    }
                    Err(e) => {
                        // Rollback uploaded parts
                        for part in parts {
                            let _ = self.telegram.delete_message(&bot.token, &self.chat_id, part.message_id).await;
                        }
                        return Err(e);
                    }
                }
            }
        }

        // 3. Save metadata
        let file_meta = File {
            id: None,
            path: path.to_string(),
            bot_id: bot.bot_id.clone(),
            parts: parts.clone(),
            total_size,
            chunked,
            hash,
            created_at: Utc::now(),
        };

        match self.store.save_file(file_meta).await {
            Ok(_) => {
                // Increment usage
                self.bot_manager.increment_usage(&bot.bot_id).await?;
                
                let _ = sender.send(UploadEvent { 
                    status: UploadStatus::Completed { file_id: parts[0].telegram_file_id.clone() } 
                }).await;
                Ok(())
            }
            Err(e) => {
                // Rollback uploaded parts from Telegram on DB failure
                for part in parts {
                    let _ = self.telegram.delete_message(&bot.token, &self.chat_id, part.message_id).await;
                }
                Err(e)
            }
        }
    }

    pub async fn rename_file(&self, old_path: &str, new_path: &str) -> Result<()> {
        self.store.rename_file(old_path, new_path).await
    }

    pub async fn delete_file(&self, path: &str) -> Result<()> {
        let file_opt: Option<File> = self.store.get_file_by_path(path).await?;
        let file = file_opt.ok_or_else(|| crate::errors::TgCloudError::FileNotFound(path.to_string()))?;

        let token = self.bot_manager.get_bot_token(&file.bot_id).await?;

        // 1. Delete all parts from Telegram
        for part in &file.parts {
            self.telegram.delete_message(&token, &self.chat_id, part.message_id).await?;
        }

        // 2. Delete from Mongo (only if all TG deletes succeeded)
        self.store.delete_file(path).await?;

        Ok(())
    }

    pub async fn download_file(&self, path: &str, output_path: &str, sender: mpsc::Sender<crate::models::DownloadEvent>) -> Result<()> {
        use crate::models::{DownloadEvent, DownloadStatus};

        let file_opt: Option<File> = self.store.get_file_by_path(path).await?;
        let file = file_opt.ok_or_else(|| crate::errors::TgCloudError::FileNotFound(path.to_string()))?;

        let _ = sender.send(DownloadEvent { status: DownloadStatus::Started { total_size: file.total_size } }).await;

        let token = self.bot_manager.get_bot_token(&file.bot_id).await
            .map_err(|e| {
                let _ = sender.try_send(DownloadEvent { status: DownloadStatus::Failed { error: e.to_string() } });
                e
            })?;

        let mut out_file = tokio::fs::File::create(output_path).await?;
        let mut downloaded_total = 0;
        
        let mut parts = file.parts;
        parts.sort_by_key(|p| p.part_number);

        for part in parts {
            let url = self.telegram.get_download_url(&token, &part.telegram_file_id).await
                 .map_err(|e| {
                    let _ = sender.try_send(DownloadEvent { status: DownloadStatus::Failed { error: e.to_string() } });
                    e
                })?;
            
            let mut response = self.telegram.download_file(&url).await
                 .map_err(|e| {
                    let _ = sender.try_send(DownloadEvent { status: DownloadStatus::Failed { error: e.to_string() } });
                    e
                })?;

            while let Some(chunk) = response.chunk().await.map_err(crate::errors::TgCloudError::TelegramError)? {
                 out_file.write_all(&chunk).await?;
                 downloaded_total += chunk.len() as u64;
                 let _ = sender.try_send(DownloadEvent { status: DownloadStatus::Progress { downloaded: downloaded_total, total: file.total_size } });
            }
        }

        let _ = sender.send(DownloadEvent { status: DownloadStatus::Completed { path: output_path.to_string() } }).await;
        Ok(())
    }

    pub async fn list_files(&self, prefix: &str) -> Result<Vec<File>> {
        self.store.list_files(prefix).await
    }
}
