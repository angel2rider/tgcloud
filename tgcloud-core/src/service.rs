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
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

/// Fixed chunk size: 256 MiB.
const CHUNK_SIZE: u64 = 268_435_456;

pub struct TgCloudService {
    store: MongoStore,
    telegram: TelegramClient,
    bot_manager: BotManager,
    chat_id: String,
    max_global_concurrency: usize,
    max_per_bot_concurrency: usize,
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
            max_global_concurrency: config.max_global_concurrency,
            max_per_bot_concurrency: config.max_per_bot_concurrency,
        })
    }

    // -----------------------------------------------------------------------
    // Helper: build per-bot semaphore map
    // -----------------------------------------------------------------------

    fn build_per_bot_semaphores(&self, bot_ids: &[String]) -> HashMap<String, Arc<Semaphore>> {
        let mut map = HashMap::with_capacity(bot_ids.len());
        for id in bot_ids {
            map.entry(id.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(self.max_per_bot_concurrency)));
        }
        map
    }

    // =======================================================================
    // Upload
    // =======================================================================

    pub async fn upload_file(&self, path: &str, sender: mpsc::Sender<UploadEvent>) -> Result<()> {
        let metadata = tokio::fs::metadata(path).await?;
        let total_size = metadata.len();
        let total_chunks = if total_size == 0 {
            1
        } else {
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

        // 1. Stream SHA-256 hash computation (single-pass, 64 KB buffer).
        let _ = sender
            .send(UploadEvent {
                status: UploadStatus::Hashing,
            })
            .await;

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

        // 2. Select bot(s).
        //    Files <= 256MB: single bot (lowest usage).
        //    Files >  256MB: distribute chunks across ALL active bots.
        let is_multi_bot = total_size > CHUNK_SIZE;

        let bots = if is_multi_bot {
            self.bot_manager.get_all_active_bots().await?
        } else {
            vec![self.bot_manager.get_upload_bot().await?]
        };

        // Build token map for quick access by spawned tasks.
        let token_map: HashMap<String, String> = bots
            .iter()
            .map(|b| (b.bot_id.clone(), b.token.clone()))
            .collect();
        let token_map = Arc::new(token_map);

        // Bot IDs list for round-robin assignment.
        let bot_ids: Vec<String> = bots.iter().map(|b| b.bot_id.clone()).collect();

        // 3. Two-layer concurrency control.
        let global_semaphore = Arc::new(Semaphore::new(self.max_global_concurrency));
        let per_bot_semaphores = Arc::new(self.build_per_bot_semaphores(&bot_ids));

        // 4. Parallel upload with bounded concurrency.
        let mut futures = FuturesUnordered::new();

        for chunk_index in 0..total_chunks {
            let offset = chunk_index as u64 * CHUNK_SIZE;
            let current_chunk_size = std::cmp::min(CHUNK_SIZE, total_size.saturating_sub(offset));

            // Assign bot: round-robin for multi-bot, single bot otherwise.
            let assigned_bot_id = bot_ids[chunk_index as usize % bot_ids.len()].clone();
            let bot_token = token_map.get(&assigned_bot_id).cloned().ok_or_else(|| {
                TgCloudError::UploadFailed(format!("Token not found for bot {}", assigned_bot_id))
            })?;

            let chunk_file_name = format!(
                "{}.chunk{}",
                std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string()),
                chunk_index
            );

            let global_sem = Arc::clone(&global_semaphore);
            let per_bot_sem =
                Arc::clone(per_bot_semaphores.get(&assigned_bot_id).ok_or_else(|| {
                    TgCloudError::UploadFailed(format!(
                        "Per-bot semaphore not found for bot {}",
                        assigned_bot_id
                    ))
                })?);
            let telegram = self.telegram.clone();
            let chat_id = self.chat_id.clone();
            let path_owned = path.to_string();
            let progress_clone = Arc::clone(&progress);
            let bot_id_owned = assigned_bot_id;

            futures.push(tokio::spawn(async move {
                // Acquire global semaphore permit.
                let _global_permit = global_sem.acquire().await.map_err(|_| {
                    TgCloudError::UploadFailed("Global semaphore closed".to_string())
                })?;

                // Acquire per-bot semaphore permit.
                let _bot_permit = per_bot_sem.acquire().await.map_err(|_| {
                    TgCloudError::UploadFailed(format!(
                        "Per-bot semaphore closed for bot {}",
                        bot_id_owned
                    ))
                })?;

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
                    bot_id: bot_id_owned,
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
            // Rollback all successfully uploaded chunks, using each chunk's bot_id.
            for chunk in &chunks {
                if let Some(token) = token_map.get(&chunk.bot_id) {
                    let _ = self
                        .telegram
                        .delete_message(token, &self.chat_id, chunk.message_id)
                        .await;
                }
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
        };

        // 6. Persist metadata.
        match self.store.save_file(file_meta).await {
            Ok(_) => {
                // Increment usage for each bot that handled chunks.
                let mut usage_counts: HashMap<String, u64> = HashMap::new();
                for chunk in &chunks {
                    *usage_counts.entry(chunk.bot_id.clone()).or_insert(0) += 1;
                }
                for (bot_id, _count) in &usage_counts {
                    self.bot_manager.increment_usage(bot_id).await?;
                }

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
                    if let Some(token) = token_map.get(&chunk.bot_id) {
                        let _ = self
                            .telegram
                            .delete_message(token, &self.chat_id, chunk.message_id)
                            .await;
                    }
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

        // Build token map from unique bot_ids across chunks.
        let unique_bot_ids: Vec<String> = {
            let mut ids: Vec<String> = file.chunks.iter().map(|c| c.bot_id.clone()).collect();
            ids.sort();
            ids.dedup();
            ids
        };

        let token_map = self
            .bot_manager
            .get_token_map(&unique_bot_ids)
            .await
            .inspect_err(|e| {
                let _ = sender.try_send(DownloadEvent {
                    status: DownloadStatus::Failed {
                        error: e.to_string(),
                    },
                });
            })?;
        let token_map = Arc::new(token_map);

        let mut chunks = file.chunks.clone();
        chunks.sort_by_key(|c| c.index);

        // 1. Two-layer concurrency control.
        let global_semaphore = Arc::new(Semaphore::new(self.max_global_concurrency));
        let per_bot_semaphores = Arc::new(self.build_per_bot_semaphores(&unique_bot_ids));

        // 2. Parallel download to temp files.
        let mut futures = FuturesUnordered::new();
        let output_path_owned = output_path.to_string();

        for chunk in &chunks {
            let global_sem = Arc::clone(&global_semaphore);
            let per_bot_sem =
                Arc::clone(per_bot_semaphores.get(&chunk.bot_id).ok_or_else(|| {
                    TgCloudError::DownloadFailed(format!(
                        "Per-bot semaphore not found for bot {}",
                        chunk.bot_id
                    ))
                })?);
            let telegram = self.telegram.clone();
            let token = token_map.get(&chunk.bot_id).cloned().ok_or_else(|| {
                TgCloudError::DownloadFailed(format!("Token not found for bot {}", chunk.bot_id))
            })?;
            let chunk_index = chunk.index;
            let telegram_file_id = chunk.telegram_file_id.clone();
            let temp_path = format!("{}.chunk_{}.tmp", output_path_owned, chunk_index);
            let progress_task = Arc::clone(&progress);
            let bot_id_owned = chunk.bot_id.clone();

            futures.push(tokio::spawn(async move {
                // Acquire global semaphore permit.
                let _global_permit = global_sem.acquire().await.map_err(|_| {
                    TgCloudError::DownloadFailed("Global semaphore closed".to_string())
                })?;

                // Acquire per-bot semaphore permit.
                let _bot_permit = per_bot_sem.acquire().await.map_err(|_| {
                    TgCloudError::DownloadFailed(format!(
                        "Per-bot semaphore closed for bot {}",
                        bot_id_owned
                    ))
                })?;

                // Get download URL and download with retry.
                let url = telegram.get_download_url(&token, &telegram_file_id).await?;
                let response = telegram
                    .download_file_with_retry(&url, Arc::clone(&progress_task))
                    .await?;

                // Stream to temp file.
                let mut temp_file = tokio::fs::File::create(&temp_path).await?;
                let mut stream = response;
                while let Some(data) = stream.chunk().await.map_err(TgCloudError::TelegramError)? {
                    temp_file.write_all(&data).await?;
                    progress_task
                        .fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
                }

                temp_file.flush().await?;
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

        // 3. Sequential merge.
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

        // 4. SHA-256 verification.
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

        // 5. Clean up temp files.
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

        // Build token map from unique bot_ids across chunks.
        let unique_bot_ids: Vec<String> = {
            let mut ids: Vec<String> = file.chunks.iter().map(|c| c.bot_id.clone()).collect();
            ids.sort();
            ids.dedup();
            ids
        };

        let token_map = self.bot_manager.get_token_map(&unique_bot_ids).await?;
        let token_map = Arc::new(token_map);

        // Two-layer concurrency control.
        let global_semaphore = Arc::new(Semaphore::new(self.max_global_concurrency));
        let per_bot_semaphores = Arc::new(self.build_per_bot_semaphores(&unique_bot_ids));

        // Parallel delete via FuturesUnordered.
        let mut futures = FuturesUnordered::new();

        for chunk in &file.chunks {
            let global_sem = Arc::clone(&global_semaphore);
            let per_bot_sem =
                Arc::clone(per_bot_semaphores.get(&chunk.bot_id).ok_or_else(|| {
                    TgCloudError::DeleteFailed(format!(
                        "Per-bot semaphore not found for bot {}",
                        chunk.bot_id
                    ))
                })?);
            let telegram = self.telegram.clone();
            let token = token_map.get(&chunk.bot_id).cloned().ok_or_else(|| {
                TgCloudError::DeleteFailed(format!("Token not found for bot {}", chunk.bot_id))
            })?;
            let chat_id = self.chat_id.clone();
            let message_id = chunk.message_id;
            let bot_id_owned = chunk.bot_id.clone();
            let chunk_index = chunk.index;

            futures.push(tokio::spawn(async move {
                // Acquire global semaphore permit.
                let _global_permit = global_sem.acquire().await.map_err(|_| {
                    TgCloudError::DeleteFailed("Global semaphore closed".to_string())
                })?;

                // Acquire per-bot semaphore permit.
                let _bot_permit = per_bot_sem.acquire().await.map_err(|_| {
                    TgCloudError::DeleteFailed(format!(
                        "Per-bot semaphore closed for bot {}",
                        bot_id_owned
                    ))
                })?;

                telegram
                    .delete_message(&token, &chat_id, message_id)
                    .await
                    .map_err(|e| {
                        TgCloudError::DeleteFailed(format!(
                            "Failed to delete chunk {} (bot {}): {}",
                            chunk_index, bot_id_owned, e
                        ))
                    })?;

                Ok::<(), TgCloudError>(())
            }));
        }

        // Collect results. ALL must succeed before metadata removal.
        let mut errors: Vec<String> = Vec::new();

        while let Some(join_result) = futures.next().await {
            match join_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    errors.push(e.to_string());
                }
                Err(join_err) => {
                    errors.push(format!("Task panicked: {}", join_err));
                }
            }
        }

        if !errors.is_empty() {
            // Partial failure: do NOT remove metadata.
            return Err(TgCloudError::DeleteFailed(format!(
                "Partial delete failure ({}/{} chunks failed): {}",
                errors.len(),
                file.chunks.len(),
                errors.join("; ")
            )));
        }

        // All Telegram deletions succeeded — remove metadata.
        self.store.delete_file(path).await?;

        Ok(())
    }

    pub async fn list_files(&self, prefix: &str) -> Result<Vec<FileMetadata>> {
        self.store.list_files(prefix).await
    }
}
