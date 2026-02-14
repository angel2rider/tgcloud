use chrono::{DateTime, Utc};
use mongodb::bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

/// A single chunk of a file stored as a Telegram document.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileChunk {
    pub index: u32,
    #[serde(default)]
    pub bot_id: Option<String>,
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
    #[serde(default)]
    pub bot_id: Option<String>,
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
    Started {
        total_size: u64,
        total_chunks: u32,
        progress: std::sync::Arc<std::sync::atomic::AtomicU64>,
    },
    Hashing,
    HashComplete {
        sha256: String,
    },
    Completed {
        file_id: String,
    },
    Failed {
        error: String,
    },
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
    Started {
        total_size: u64,
        total_chunks: u32,
        progress: std::sync::Arc<std::sync::atomic::AtomicU64>,
    },
    Merging,
    Verifying,
    Completed {
        path: String,
    },
    Failed {
        error: String,
    },
}
