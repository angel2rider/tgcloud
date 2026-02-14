use serde::{Deserialize, Serialize};
use mongodb::bson::oid::ObjectId;
use chrono::{DateTime, Utc};

/// A single chunk of a file stored as a Telegram document.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileChunk {
    pub index: u32,
    pub telegram_file_id: String,
    pub message_id: i64,
    pub size: u64,
}

/// Metadata for a file stored across one or more Telegram documents.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileMetadata {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub file_id: String,
    pub original_name: String,
    pub size: u64,
    pub chunk_size: u64,
    pub total_chunks: u32,
    pub sha256: String,
    pub chunks: Vec<FileChunk>,
    pub created_at: DateTime<Utc>,
    pub bot_id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Bot {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub bot_id: String,
    pub token: String,
    pub upload_count: u64,
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Upload events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UploadEvent {
    pub status: UploadStatus,
}

#[derive(Debug, Clone)]
pub enum UploadStatus {
    Started,
    Hashing,
    HashComplete { sha256: String },
    ChunkStarted { chunk_index: u32, total_chunks: u32, chunk_size: u64 },
    ChunkProgress { chunk_index: u32, bytes_sent: u64, chunk_size: u64 },
    ChunkCompleted { chunk_index: u32, total_chunks: u32 },
    Completed { file_id: String },
    Failed { error: String },
}

// ---------------------------------------------------------------------------
// Download events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DownloadEvent {
    pub status: DownloadStatus,
}

#[derive(Debug, Clone)]
pub enum DownloadStatus {
    Started { total_size: u64, total_chunks: u32 },
    ChunkStarted { chunk_index: u32, total_chunks: u32, chunk_size: u64 },
    ChunkProgress { chunk_index: u32, bytes_downloaded: u64, chunk_size: u64 },
    ChunkCompleted { chunk_index: u32, total_chunks: u32 },
    Merging,
    Verifying,
    Completed { path: String },
    Failed { error: String },
}
