use crate::bot_manager::BotManager;
use crate::errors::{Result, TgCloudError};
use crate::models::{
    DownloadEvent, DownloadStatus, FileChunk, FileMetadata, UploadEvent, UploadStatus,
};
use crate::storage::MongoStore;
use crate::telegram_client::TelegramClient;

use chrono::Utc;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

/// Fixed chunk size: 256 MiB.
const CHUNK_SIZE: u64 = 268_435_456;
/// Maximum parallel upload tasks.
const MAX_PARALLEL_UPLOADS: usize = 4;
/// Maximum parallel download tasks.
const MAX_PARALLEL_DOWNLOADS: usize = 6;

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

    // =======================================================================
    // Upload
    // =======================================================================

    pub async fn upload_file(&self, path: &str, sender: mpsc::Sender<UploadEvent>) -> Result<()> {
        let _ = sender
            .send(UploadEvent {
                status: UploadStatus::Started,
            })
            .await;

        // 1. Stream SHA-256 hash computation (single-pass, 64 KB buffer).
        let _ = sender
            .send(UploadEvent {
                status: UploadStatus::Hashing,
            })
            .await;

        let metadata = tokio::fs::metadata(path).await?;
        let total_size = metadata.len();

        let sha256 = {
            let mut hasher = Sha256::new();
            let mut file_for_hash = tokio::fs::File::open(path).await?;
            let mut buf = [0u8; 65_536]; // 64 KB
            loop {
                let n = file_for_hash.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            hex::encode(hasher.finalize())
        };

        let _ = sender
            .send(UploadEvent {
                status: UploadStatus::HashComplete {
                    sha256: sha256.clone(),
                },
            })
            .await;

        // 2. Determine chunks.
        let total_chunks = if total_size == 0 {
            1
        } else {
            ((total_size + CHUNK_SIZE - 1) / CHUNK_SIZE) as u32
        };

        // 3. Select bot.
        let bot = self.bot_manager.get_upload_bot().await?;

        // 4. Parallel upload with bounded concurrency.
        let semaphore = Arc::new(Semaphore::new(MAX_PARALLEL_UPLOADS));
        let mut futures = FuturesUnordered::new();

        for chunk_index in 0..total_chunks {
            let offset = chunk_index as u64 * CHUNK_SIZE;
            let current_chunk_size = std::cmp::min(CHUNK_SIZE, total_size.saturating_sub(offset));

            let chunk_file_name = format!(
                "{}.chunk{}",
                std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string()),
                chunk_index
            );

            let permit = Arc::clone(&semaphore);
            let telegram = self.telegram.clone();
            let bot_token = bot.token.clone();
            let chat_id = self.chat_id.clone();
            let sender_clone = sender.clone();
            let path_owned = path.to_string();

            futures.push(tokio::spawn(async move {
                // Acquire semaphore permit — bounds concurrent tasks.
                let _permit = permit
                    .acquire()
                    .await
                    .map_err(|_| TgCloudError::UploadFailed("Semaphore closed".to_string()))?;

                let _ = sender_clone
                    .send(UploadEvent {
                        status: UploadStatus::ChunkStarted {
                            chunk_index,
                            total_chunks,
                            chunk_size: current_chunk_size,
                        },
                    })
                    .await;

                // Upload with retry — each retry re-opens the file and seeks.
                let (tg_id, msg_id) = telegram
                    .upload_part_with_retry(
                        &bot_token,
                        &chat_id,
                        chunk_file_name,
                        &path_owned,
                        offset,
                        current_chunk_size,
                    )
                    .await?;

                let _ = sender_clone
                    .send(UploadEvent {
                        status: UploadStatus::ChunkCompleted {
                            chunk_index,
                            total_chunks,
                        },
                    })
                    .await;

                Ok::<FileChunk, TgCloudError>(FileChunk {
                    index: chunk_index,
                    telegram_file_id: tg_id,
                    message_id: msg_id,
                    size: current_chunk_size,
                })
            }));
        }

        // Collect results; on any error, rollback already-uploaded chunks.
        let mut chunks: Vec<FileChunk> = Vec::with_capacity(total_chunks as usize);
        let mut first_error: Option<TgCloudError> = None;

        while let Some(join_result) = futures.next().await {
            match join_result {
                Ok(Ok(chunk)) => {
                    chunks.push(chunk);
                }
                Ok(Err(e)) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                    // Continue draining to allow rollback of completed chunks.
                }
                Err(join_err) => {
                    if first_error.is_none() {
                        first_error = Some(TgCloudError::UploadFailed(format!(
                            "Task panicked: {}",
                            join_err
                        )));
                    }
                }
            }
        }

        if let Some(err) = first_error {
            // Rollback all successfully uploaded chunks.
            for chunk in &chunks {
                let _ = self
                    .telegram
                    .delete_message(&bot.token, &self.chat_id, chunk.message_id)
                    .await;
            }
            let _ = sender
                .send(UploadEvent {
                    status: UploadStatus::Failed {
                        error: err.to_string(),
                    },
                })
                .await;
            return Err(err);
        }

        // 5. Sort chunks by index before insert.
        chunks.sort_by_key(|c| c.index);

        let file_id = Uuid::new_v4().to_string();
        let original_name = path.to_string();

        let file_meta = FileMetadata {
            id: None,
            file_id: file_id.clone(),
            original_name,
            size: total_size,
            chunk_size: CHUNK_SIZE,
            total_chunks,
            sha256,
            chunks: chunks.clone(),
            created_at: Utc::now(),
            bot_id: bot.bot_id.clone(),
        };

        // 6. Persist metadata.
        match self.store.save_file(file_meta).await {
            Ok(_) => {
                self.bot_manager.increment_usage(&bot.bot_id).await?;
                let _ = sender
                    .send(UploadEvent {
                        status: UploadStatus::Completed { file_id },
                    })
                    .await;
                Ok(())
            }
            Err(e) => {
                // Rollback Telegram uploads on DB failure.
                for chunk in &chunks {
                    let _ = self
                        .telegram
                        .delete_message(&bot.token, &self.chat_id, chunk.message_id)
                        .await;
                }
                let _ = sender
                    .send(UploadEvent {
                        status: UploadStatus::Failed {
                            error: e.to_string(),
                        },
                    })
                    .await;
                Err(e)
            }
        }
    }

    // =======================================================================
    // Download
    // =======================================================================

    pub async fn download_file(
        &self,
        path: &str,
        output_path: &str,
        sender: mpsc::Sender<DownloadEvent>,
    ) -> Result<()> {
        let file_opt = self.store.get_file_by_path(path).await?;
        let file = file_opt.ok_or_else(|| TgCloudError::FileNotFound(path.to_string()))?;

        let _ = sender
            .send(DownloadEvent {
                status: DownloadStatus::Started {
                    total_size: file.size,
                    total_chunks: file.total_chunks,
                },
            })
            .await;

        let token = self
            .bot_manager
            .get_bot_token(&file.bot_id)
            .await
            .map_err(|e| {
                let _ = sender.try_send(DownloadEvent {
                    status: DownloadStatus::Failed {
                        error: e.to_string(),
                    },
                });
                e
            })?;

        let mut chunks = file.chunks.clone();
        chunks.sort_by_key(|c| c.index);

        // 1. Parallel download to temp files.
        let semaphore = Arc::new(Semaphore::new(MAX_PARALLEL_DOWNLOADS));
        let mut futures = FuturesUnordered::new();
        let output_path_owned = output_path.to_string();

        for chunk in &chunks {
            let permit = Arc::clone(&semaphore);
            let telegram = self.telegram.clone();
            let token_clone = token.clone();
            let sender_clone = sender.clone();
            let chunk_index = chunk.index;
            let chunk_size = chunk.size;
            let total_chunks = file.total_chunks;
            let telegram_file_id = chunk.telegram_file_id.clone();
            let temp_path = format!("{}.chunk_{}.tmp", output_path_owned, chunk_index);

            futures.push(tokio::spawn(async move {
                let _permit = permit
                    .acquire()
                    .await
                    .map_err(|_| TgCloudError::DownloadFailed("Semaphore closed".to_string()))?;

                let _ = sender_clone
                    .send(DownloadEvent {
                        status: DownloadStatus::ChunkStarted {
                            chunk_index,
                            total_chunks,
                            chunk_size,
                        },
                    })
                    .await;

                // Get download URL and download with retry.
                let url = telegram
                    .get_download_url(&token_clone, &telegram_file_id)
                    .await?;
                let mut response = telegram.download_file_with_retry(&url).await?;

                // Stream to temp file.
                let mut temp_file = tokio::fs::File::create(&temp_path).await?;
                let mut bytes_written: u64 = 0;

                while let Some(data) = response
                    .chunk()
                    .await
                    .map_err(TgCloudError::TelegramError)?
                {
                    temp_file.write_all(&data).await?;
                    bytes_written += data.len() as u64;
                    let _ = sender_clone.try_send(DownloadEvent {
                        status: DownloadStatus::ChunkProgress {
                            chunk_index,
                            bytes_downloaded: bytes_written,
                            chunk_size,
                        },
                    });
                }

                temp_file.flush().await?;

                let _ = sender_clone
                    .send(DownloadEvent {
                        status: DownloadStatus::ChunkCompleted {
                            chunk_index,
                            total_chunks,
                        },
                    })
                    .await;

                Ok::<(u32, String), TgCloudError>((chunk_index, temp_path))
            }));
        }

        // Collect results.
        let mut temp_files: Vec<(u32, String)> = Vec::with_capacity(chunks.len());
        let mut first_error: Option<TgCloudError> = None;

        while let Some(join_result) = futures.next().await {
            match join_result {
                Ok(Ok(entry)) => temp_files.push(entry),
                Ok(Err(e)) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
                Err(join_err) => {
                    if first_error.is_none() {
                        first_error = Some(TgCloudError::DownloadFailed(format!(
                            "Task panicked: {}",
                            join_err
                        )));
                    }
                }
            }
        }

        // Clean up on error.
        if let Some(err) = first_error {
            for (_, tmp) in &temp_files {
                let _ = tokio::fs::remove_file(tmp).await;
            }
            let _ = sender
                .send(DownloadEvent {
                    status: DownloadStatus::Failed {
                        error: err.to_string(),
                    },
                })
                .await;
            return Err(err);
        }

        // 2. Sequential merge.
        let _ = sender
            .send(DownloadEvent {
                status: DownloadStatus::Merging,
            })
            .await;

        temp_files.sort_by_key(|(idx, _)| *idx);

        let mut out_file = tokio::fs::File::create(output_path).await?;
        for (_, tmp_path) in &temp_files {
            let mut tmp = tokio::fs::File::open(tmp_path).await?;
            let mut buf = [0u8; 65_536]; // 64 KB
            loop {
                let n = tmp.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                out_file.write_all(&buf[..n]).await?;
            }
        }
        out_file.flush().await?;
        drop(out_file);

        // 3. SHA-256 verification.
        let _ = sender
            .send(DownloadEvent {
                status: DownloadStatus::Verifying,
            })
            .await;

        let actual_hash = {
            let mut hasher = Sha256::new();
            let mut f = tokio::fs::File::open(output_path).await?;
            let mut buf = [0u8; 65_536];
            loop {
                let n = f.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            hex::encode(hasher.finalize())
        };

        if actual_hash != file.sha256 {
            // Remove corrupt file.
            let _ = tokio::fs::remove_file(output_path).await;
            for (_, tmp) in &temp_files {
                let _ = tokio::fs::remove_file(tmp).await;
            }
            let err = TgCloudError::IntegrityError {
                expected: file.sha256.clone(),
                got: actual_hash,
            };
            let _ = sender
                .send(DownloadEvent {
                    status: DownloadStatus::Failed {
                        error: err.to_string(),
                    },
                })
                .await;
            return Err(err);
        }

        // 4. Clean up temp files.
        for (_, tmp) in &temp_files {
            let _ = tokio::fs::remove_file(tmp).await;
        }

        let _ = sender
            .send(DownloadEvent {
                status: DownloadStatus::Completed {
                    path: output_path.to_string(),
                },
            })
            .await;

        Ok(())
    }

    // =======================================================================
    // Rename / Delete / List
    // =======================================================================

    pub async fn rename_file(&self, old_path: &str, new_path: &str) -> Result<()> {
        self.store.rename_file(old_path, new_path).await
    }

    pub async fn delete_file(&self, path: &str) -> Result<()> {
        let file_opt: Option<FileMetadata> = self.store.get_file_by_path(path).await?;
        let file = file_opt.ok_or_else(|| TgCloudError::FileNotFound(path.to_string()))?;

        let token = self.bot_manager.get_bot_token(&file.bot_id).await?;

        // Delete all chunks from Telegram.
        for chunk in &file.chunks {
            self.telegram
                .delete_message(&token, &self.chat_id, chunk.message_id)
                .await?;
        }

        // Delete from Mongo (only if all TG deletes succeeded).
        self.store.delete_file(path).await?;

        Ok(())
    }

    pub async fn list_files(&self, prefix: &str) -> Result<Vec<FileMetadata>> {
        self.store.list_files(prefix).await
    }
}
