use askama::Template;
use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tgcloud_core::{FileMetadata, TgCloudService};
use tokio::sync::mpsc;
use tower_http::cors::CorsLayer;

#[derive(Clone)]
pub struct WebState {
    pub service: Arc<TgCloudService>,
}

#[derive(Serialize)]
struct FileInfo {
    file_id: String,
    original_name: String,
    size: String,
    created_at: String,
    sha256: String,
    total_chunks: u32,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    files: Vec<FileInfo>,
}

pub async fn start_server(service: Arc<TgCloudService>) -> anyhow::Result<()> {
    let state = WebState { service };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/files", get(list_files_handler))
        .route("/api/upload", post(upload_handler))
        .route("/api/download", post(download_handler))
        .route("/api/rename", post(rename_handler))
        .route("/api/file/:path", delete(delete_file_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 8090));
    println!(
        "\n  {} TGCloud Web UI running at http://{}",
        "🌐".cyan(),
        addr
    );

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install CTRL+C handler");
        })
        .await?;

    Ok(())
}

fn format_file_info(f: FileMetadata) -> FileInfo {
    FileInfo {
        file_id: f.file_id,
        original_name: std::path::Path::new(&f.original_name)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(f.original_name),
        size: human_bytes::human_bytes(f.size as f64),
        created_at: f.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
        sha256: f.sha256,
        total_chunks: f.total_chunks,
    }
}

async fn index_handler(State(state): State<WebState>) -> impl IntoResponse {
    match state.service.list_files("root").await {
        Ok(files) => {
            let files = files.into_iter().map(format_file_info).collect();
            let template = IndexTemplate { files };
            match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Template error: {}", e),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Service error: {}", e),
        )
            .into_response(),
    }
}

async fn list_files_handler(State(state): State<WebState>) -> impl IntoResponse {
    match state.service.list_files("root").await {
        Ok(files) => {
            let files: Vec<FileInfo> = files.into_iter().map(format_file_info).collect();
            Json(files).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct RenameRequest {
    file_id: String,
    new_path: String,
}

async fn rename_handler(
    State(state): State<WebState>,
    Json(payload): Json<RenameRequest>,
) -> impl IntoResponse {
    match state
        .service
        .rename_file_by_id(&payload.file_id, &payload.new_path)
        .await
    {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn delete_file_handler(
    State(state): State<WebState>,
    Path(file_id): Path<String>,
) -> impl IntoResponse {
    match state.service.delete_file_by_id(&file_id).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct DownloadRequest {
    remote_path: String,
}

async fn download_handler(
    State(state): State<WebState>,
    Json(payload): Json<DownloadRequest>,
) -> impl IntoResponse {
    let (tx, _rx) = mpsc::channel(100);
    let service = state.service.clone();
    let path = payload.remote_path.clone();

    tokio::spawn(async move {
        let _ = service.download_file(&path, tx).await;
    });

    StatusCode::ACCEPTED.into_response()
}

async fn upload_handler(
    State(state): State<WebState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        if let Some(filename) = field.file_name() {
            let filename = filename.to_string();
            let data = match field.bytes().await {
                Ok(b) => b,
                Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
            };

            let temp_dir = std::env::temp_dir();
            let temp_path = temp_dir.join(&filename);
            if let Err(e) = tokio::fs::write(&temp_path, &data).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
            }

            let (tx, _rx) = mpsc::channel(100);
            let service = state.service.clone();
            let temp_path_str = temp_path.to_string_lossy().to_string();

            tokio::spawn(async move {
                let _ = service.upload_file(&temp_path_str, tx).await;
                let _ = tokio::fs::remove_file(&temp_path_str).await;
            });

            return StatusCode::ACCEPTED.into_response();
        }
    }

    StatusCode::BAD_REQUEST.into_response()
}
