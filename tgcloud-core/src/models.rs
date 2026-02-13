use serde::{Deserialize, Serialize};
use mongodb::bson::oid::ObjectId;
use chrono::{DateTime, Utc};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct File {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub path: String,
    pub bot_id: String,
    pub telegram_file_id: String,
    #[serde(default)] // Handle missing field for legacy data
    pub message_id: Option<i64>,
    pub size: u64,
    pub hash: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Bot {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub bot_id: String, // from BotFather (ID part of token)
    pub token: String,
    pub upload_count: u64,
    pub active: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DownloadEvent {
    pub status: DownloadStatus,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum DownloadStatus {
    Started { total_size: u64 },
    Progress { downloaded: u64, total: u64 },
    Completed { path: String },
    Failed { error: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UploadEvent {
    pub status: UploadStatus,
}


#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum UploadStatus {
    Started,
    Progress { sent: u64, total: u64 },
    Completed { file_id: String },
    Failed { error: String },
}
