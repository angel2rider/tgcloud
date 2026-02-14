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
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

/// Fixed chunk size: 2 GiB (optimized for local Telegram Bot API).
const CHUNK_SIZE: u64 = 2_147_483_648;

pub struct TgCloudService {
    store: MongoStore,
    telegram: TelegramClient,
    bot_id: String,
    bot_token: String,
    chat_id: String,
    max_concurrency: usize,
}

impl TgCloudService {
    pub async fn new(config: crate::config::Config) -> Result<Self> {
        let store = MongoStore::new(&config.mongo_uri).await?;
        let telegram = TelegramClient::new(config.telegram_api_url.clone());

        Ok(Self {
            store,
            telegram,
            bot_id: config.bot_id,
            bot_token: config.bot_token,
            chat_id: config.telegram_chat_id,
            max_concurrency: config.max_concurrency,
        })
    }

    // =======================================================================
    // Upload
    // =======================================================================

    pub async fn upload_file(&self, path: &str, sender: mpsc::Sender<UploadEvent>) -> Result<()> {
        let metadata = tokio::fs::metadata(path).await?;
        let total_size = metadata.len();

        // Chunk if > 2GB
        let total_chunks = if total_size == 0 {
            1
        } else {
            // Use manual division or handle clippy warnings if necessary
            total_size.div_ceil(CHUNK_SIZE) as u32
        };

        let progress = Arc::new(AtomicU64::new(0));

        let _ = sender
            .send(UploadEvent {
                status: UploadStatus::Started {
                    total_size,
                    total_chunks,
                    progress: Arc::clone(&progress),
                },
            })
            .await;

        // Hash the full file once for verification
        let _ = sender
            .send(UploadEvent {
                status: UploadStatus::Hashing,
            })
            .await;

        let sha256 = {
            let mut hasher = Sha256::new();
            let mut file_for_hash = tokio::fs::File::open(path).await?;
            let mut buf = [0u8; 65_536];
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

        // Parallelism allowed for large files (> 256MB total)
        // Note: For chunked uploads (> 2GB), we definitely use it.
        let semaphore = Arc::new(Semaphore::new(self.max_concurrency));
        let mut futures = FuturesUnordered::new();

        for chunk_index in 0..total_chunks {
            let offset = chunk_index as u64 * CHUNK_SIZE;
            let current_chunk_size = std::cmp::min(CHUNK_SIZE, total_size.saturating_sub(offset));

            let chunk_file_name = if total_chunks == 1 {
                std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string())
            } else {
                format!(
                    "{}.chunk{}",
                    std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".to_string()),
                    chunk_index
                )
            };

            let sem = Arc::clone(&semaphore);
            let telegram = self.telegram.clone();
            let bot_token = self.bot_token.clone();
            let bot_id = self.bot_id.clone();
            let chat_id = self.chat_id.clone();
            let path_owned = path.to_string();
            let progress_clone = Arc::clone(&progress);

            futures.push(tokio::spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|_| TgCloudError::UploadFailed("Semaphore closed".to_string()))?;

                let (tg_id, msg_id) = telegram
                    .upload_part_with_retry(
                        &bot_token,
                        &chat_id,
                        chunk_file_name,
                        &path_owned,
                        offset,
                        current_chunk_size,
                        progress_clone,
                    )
                    .await?;

                Ok::<FileChunk, TgCloudError>(FileChunk {
                    index: chunk_index,
                    bot_id: Some(bot_id),
                    telegram_file_id: tg_id,
                    message_id: msg_id,
                    size: current_chunk_size,
                })
            }));
        }

        let mut chunks: Vec<FileChunk> = Vec::with_capacity(total_chunks as usize);
        let mut first_error: Option<TgCloudError> = None;

        while let Some(join_result) = futures.next().await {
            match join_result {
                Ok(Ok(chunk)) => chunks.push(chunk),
                Ok(Err(e)) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
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
            for chunk in &chunks {
                let _ = self
                    .telegram
                    .delete_message(&self.bot_token, &self.chat_id, chunk.message_id)
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
            bot_id: Some(self.bot_id.clone()),
        };

        match self.store.save_file(file_meta).await {
            Ok(_) => {
                let _ = sender
                    .send(UploadEvent {
                        status: UploadStatus::Completed { file_id },
                    })
                    .await;
                Ok(())
            }
            Err(e) => {
                for chunk in &chunks {
                    let _ = self
                        .telegram
                        .delete_message(&self.bot_token, &self.chat_id, chunk.message_id)
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
    // Download (Local Fetch Only)
    // =======================================================================

    pub async fn download_file(
        &self,
        path: &str,
        sender: mpsc::Sender<DownloadEvent>,
    ) -> Result<()> {
        let file_opt: Option<FileMetadata> = self.store.get_file_by_path(path).await?;
        let file = file_opt.ok_or_else(|| TgCloudError::FileNotFound(path.to_string()))?;

        let progress = Arc::new(AtomicU64::new(0));

        let _ = sender
            .send(DownloadEvent {
                status: DownloadStatus::Started {
                    total_size: file.size,
                    total_chunks: file.total_chunks,
                    progress: Arc::clone(&progress),
                },
            })
            .await;

        let mut chunk_paths: Vec<String> = Vec::new();

        // Sequential download for local fetch (files stay on server)
        for chunk in &file.chunks {
            let file_path = self
                .telegram
                .get_local_file_path(&self.bot_token, &chunk.telegram_file_id)
                .await?;

            // In local mode, getFile returns the absolute path on disk.
            chunk_paths.push(file_path);

            // Increment progress by chunk size immediately as it's "fetched" to local cache
            progress.fetch_add(chunk.size, std::sync::atomic::Ordering::Relaxed);
        }

        let _ = sender
            .send(DownloadEvent {
                status: DownloadStatus::Merging,
            })
            .await;

        let original_filename = std::path::Path::new(&file.original_name)
            .file_name()
            .ok_or_else(|| TgCloudError::DownloadFailed("Invalid original name".to_string()))?
            .to_string_lossy();

        // Merge chunks in the server's documents directory if multiple chunks exist
        let final_path = if chunk_paths.len() > 1 {
            let first_path = &chunk_paths[0];
            let parent = std::path::Path::new(first_path)
                .parent()
                .ok_or_else(|| TgCloudError::DownloadFailed("Invalid chunk path".to_string()))?;
            let merged_path = parent.join(original_filename.as_ref());

            let mut out_file = tokio::fs::File::create(&merged_path).await?;
            for tmp_path in &chunk_paths {
                let mut tmp = tokio::fs::File::open(tmp_path).await?;
                let mut buf = [0u8; 65_536];
                loop {
                    let n = tmp.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    out_file.write_all(&buf[..n]).await?;
                }
            }
            out_file.flush().await?;
            merged_path.to_string_lossy().to_string()
        } else {
            // Rename to original filename
            let current_path = &chunk_paths[0];
            let parent = std::path::Path::new(current_path)
                .parent()
                .ok_or_else(|| TgCloudError::DownloadFailed("Invalid chunk path".to_string()))?;
            let target_path = parent.join(original_filename.as_ref());
            let target_path_str = target_path.to_string_lossy().to_string();

            if current_path != &target_path_str {
                tokio::fs::rename(current_path, &target_path).await?;
            }
            target_path_str
        };

        let _ = sender
            .send(DownloadEvent {
                status: DownloadStatus::Verifying,
            })
            .await;

        // Verify SHA-256 of the FULL file (single chunk or merged)
        let actual_hash = {
            let mut hasher = Sha256::new();
            let mut f = tokio::fs::File::open(&final_path).await?;
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
            let err = TgCloudError::IntegrityFailed(format!(
                "SHA256 mismatch: expected {}, got {}",
                file.sha256, actual_hash
            ));
            let _ = sender
                .send(DownloadEvent {
                    status: DownloadStatus::Failed {
                        error: err.to_string(),
                    },
                })
                .await;
            return Err(err);
        }

        let _ = sender
            .send(DownloadEvent {
                status: DownloadStatus::Completed { path: final_path },
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

    pub async fn rename_file_by_id(&self, file_id: &str, new_name: &str) -> Result<()> {
        self.store.rename_file_by_id(file_id, new_name).await
    }

    pub async fn delete_file_by_id(&self, file_id: &str) -> Result<()> {
        let file_opt: Option<FileMetadata> = self.store.get_file_by_id(file_id).await?;
        let file = file_opt.ok_or_else(|| TgCloudError::FileNotFound(file_id.to_string()))?;

        self.delete_file_internal(file).await
    }

    pub async fn delete_file(&self, path: &str) -> Result<()> {
        let file_opt: Option<FileMetadata> = self.store.get_file_by_path(path).await?;
        let file = file_opt.ok_or_else(|| TgCloudError::FileNotFound(path.to_string()))?;

        self.delete_file_internal(file).await
    }

    async fn delete_file_internal(&self, file: FileMetadata) -> Result<()> {
        let semaphore = Arc::new(Semaphore::new(self.max_concurrency));
        let mut futures = FuturesUnordered::new();

        for chunk in &file.chunks {
            let sem = Arc::clone(&semaphore);
            let telegram = self.telegram.clone();
            let bot_token = self.bot_token.clone();
            let chat_id = self.chat_id.clone();
            let message_id = chunk.message_id;
            let chunk_index = chunk.index;

            futures.push(tokio::spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|_| TgCloudError::DeleteFailed("Semaphore closed".to_string()))?;

                telegram
                    .delete_message(&bot_token, &chat_id, message_id)
                    .await
                    .map_err(|e| {
                        TgCloudError::DeleteFailed(format!(
                            "Failed to delete chunk {}: {}",
                            chunk_index, e
                        ))
                    })?;

                Ok::<(), TgCloudError>(())
            }));
        }

        let mut errors: Vec<String> = Vec::new();

        while let Some(join_result) = futures.next().await {
            match join_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => errors.push(e.to_string()),
                Err(join_err) => errors.push(format!("Task panicked: {}", join_err)),
            }
        }

        if !errors.is_empty() {
            return Err(TgCloudError::DeleteFailed(format!(
                "Partial delete failure ({}/{} chunks failed): {}",
                errors.len(),
                file.chunks.len(),
                errors.join("; ")
            )));
        }

        self.store.delete_file_by_id(&file.file_id).await?;

        Ok(())
    }

    pub async fn list_files(&self, prefix: &str) -> Result<Vec<FileMetadata>> {
        self.store.list_files(prefix).await
    }
}
