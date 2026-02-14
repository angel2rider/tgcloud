mod ui;

use clap::{Parser, Subcommand};
use dotenv::dotenv;
use std::collections::HashMap;
use std::env;
use tgcloud_core::{
    BotConfig, Config, DownloadStatus, TgCloudService, UploadStatus,
};
use tokio::sync::mpsc;
use anyhow::Context;
use ui::*;
use owo_colors::OwoColorize;
use indicatif::ProgressBar;

#[derive(Parser)]
#[command(name = "tgcloud")]
#[command(about = "Telegram Cloud Storage CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Upload a file
    Upload { path: String },
    /// Download a file
    Download {
        remote_path: String,
        local_path: String,
    },
    /// List files
    List {
        #[arg(default_value = "root")]
        folder: String,
    },
    /// Rename a file
    Rename {
        old_path: String,
        new_path: String,
    },
    /// Delete a file
    Delete { path: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();

    let args = Cli::parse();

    print_banner();

    // Load configuration
    let mongo_uri = env::var("MONGO_URI").context("MONGO_URI must be set")?;
    let telegram_api_url =
        env::var("TELEGRAM_API_URL").unwrap_or_else(|_| "http://localhost:8081".to_string());
    let telegram_chat_id =
        env::var("TELEGRAM_CHAT_ID").context("TELEGRAM_CHAT_ID must be set")?;

    let mut bots = Vec::new();
    if let Ok(bots_json) = env::var("BOTS_JSON") {
        bots = serde_json::from_str(&bots_json).context("Failed to parse BOTS_JSON")?;
    } else if let (Ok(id), Ok(token)) = (env::var("BOT_ID"), env::var("BOT_TOKEN")) {
        bots.push(BotConfig {
            bot_id: id,
            token,
        });
    }

    let config = Config {
        mongo_uri,
        telegram_api_url,
        telegram_chat_id,
        bots,
    };

    let spinner = create_spinner("Connecting to services...");
    let service = TgCloudService::new(config)
        .await
        .map_err(|e| {
            spinner.finish_and_clear();
            e
        })
        .context("Failed to initialize service")?;
    spinner.finish_and_clear();

    match args.command {
        // ===================================================================
        // Upload
        // ===================================================================
        Commands::Upload { path } => {
            println!("🚀 Starting upload for: {}", path.cyan());
            let (tx, mut rx) = mpsc::channel(256);

            let service_handle = service;
            let upload_handle = tokio::spawn(async move {
                service_handle.upload_file(&path, tx).await
            });

            let mp = create_multi_progress();
            let mut overall_bar: Option<ProgressBar> = None;
            let mut chunk_bars: HashMap<u32, ProgressBar> = HashMap::new();
            let mut spinner_bar: Option<ProgressBar> = None;
            let mut total_file_size: u64;
            let mut overall_sent: u64 = 0;

            while let Some(event) = rx.recv().await {
                match event.status {
                    UploadStatus::Started => {
                        let s = mp.add(create_spinner("Preparing upload..."));
                        spinner_bar = Some(s);
                    }
                    UploadStatus::Hashing => {
                        if let Some(s) = spinner_bar.take() {
                            s.finish_and_clear();
                        }
                        let s = mp.add(create_spinner("Calculating SHA-256 hash..."));
                        spinner_bar = Some(s);
                    }
                    UploadStatus::HashComplete { sha256 } => {
                        if let Some(s) = spinner_bar.take() {
                            s.finish_and_clear();
                        }
                        mp.println(format!(
                            "  {} SHA-256: {}",
                            "🔒".cyan(),
                            sha256[..16].to_string().yellow()
                        ))
                        .ok();
                    }
                    UploadStatus::ChunkStarted {
                        chunk_index,
                        total_chunks,
                        chunk_size,
                    } => {
                        // Create overall bar on first chunk if not yet created.
                        if overall_bar.is_none() {
                            // Compute total size from chunks.
                            // We accumulate as we go. Set estimated total.
                            total_file_size = chunk_size * total_chunks as u64; // rough estimate
                            let ob = create_overall_bar(&mp, total_file_size);
                            overall_bar = Some(ob);
                        }
                        let cb = create_chunk_bar(&mp, chunk_index, total_chunks, chunk_size);
                        chunk_bars.insert(chunk_index, cb);
                    }
                    UploadStatus::ChunkProgress {
                        chunk_index,
                        bytes_sent,
                        chunk_size: _,
                    } => {
                        if let Some(cb) = chunk_bars.get(&chunk_index) {
                            cb.set_position(bytes_sent);
                        }
                    }
                    UploadStatus::ChunkCompleted {
                        chunk_index,
                        total_chunks: _,
                    } => {
                        if let Some(cb) = chunk_bars.remove(&chunk_index) {
                            let chunk_size = cb.length().unwrap_or(0);
                            cb.finish_and_clear();
                            overall_sent += chunk_size;
                            if let Some(ob) = &overall_bar {
                                ob.set_position(overall_sent);
                            }
                        }
                    }
                    UploadStatus::Completed { file_id } => {
                        if let Some(ob) = overall_bar.take() {
                            ob.finish_and_clear();
                        }
                        print_success(&format!(
                            "Upload completed!\n    File ID: {}\n",
                            file_id
                        ));
                    }
                    UploadStatus::Failed { error } => {
                        if let Some(ob) = overall_bar.take() {
                            ob.finish_and_clear();
                        }
                        for (_, cb) in chunk_bars.drain() {
                            cb.finish_and_clear();
                        }
                        print_error(&format!("Upload failed: {}", error));
                    }
                }
            }

            if let Err(e) = upload_handle.await? {
                print_error(&e.to_string());
            }
        }

        // ===================================================================
        // Download
        // ===================================================================
        Commands::Download {
            remote_path,
            local_path,
        } => {
            println!(
                "📥 Downloading {} to {}",
                remote_path.cyan(),
                local_path.green()
            );

            let (tx, mut rx) = mpsc::channel(256);
            let service_handle = service;

            let download_handle = tokio::spawn(async move {
                service_handle
                    .download_file(&remote_path, &local_path, tx)
                    .await
            });

            let mp = create_multi_progress();
            let mut overall_bar: Option<ProgressBar> = None;
            let mut chunk_bars: HashMap<u32, ProgressBar> = HashMap::new();
            let mut spinner_bar: Option<ProgressBar> = None;
            let mut overall_downloaded: u64 = 0;

            while let Some(event) = rx.recv().await {
                match event.status {
                    DownloadStatus::Started {
                        total_size,
                        total_chunks,
                    } => {
                        let ob = create_overall_bar(&mp, total_size);
                        overall_bar = Some(ob);
                        mp.println(format!(
                            "  {} File: {} in {} chunk(s)",
                            "📁".cyan(),
                            human_bytes::human_bytes(total_size as f64).yellow(),
                            total_chunks.to_string().green()
                        ))
                        .ok();
                    }
                    DownloadStatus::ChunkStarted {
                        chunk_index,
                        total_chunks,
                        chunk_size,
                    } => {
                        let cb =
                            create_chunk_bar(&mp, chunk_index, total_chunks, chunk_size);
                        chunk_bars.insert(chunk_index, cb);
                    }
                    DownloadStatus::ChunkProgress {
                        chunk_index,
                        bytes_downloaded,
                        chunk_size: _,
                    } => {
                        if let Some(cb) = chunk_bars.get(&chunk_index) {
                            cb.set_position(bytes_downloaded);
                        }
                    }
                    DownloadStatus::ChunkCompleted {
                        chunk_index,
                        total_chunks: _,
                    } => {
                        if let Some(cb) = chunk_bars.remove(&chunk_index) {
                            let chunk_size = cb.length().unwrap_or(0);
                            cb.finish_and_clear();
                            overall_downloaded += chunk_size;
                            if let Some(ob) = &overall_bar {
                                ob.set_position(overall_downloaded);
                            }
                        }
                    }
                    DownloadStatus::Merging => {
                        let s = mp.add(create_spinner("Merging chunks..."));
                        spinner_bar = Some(s);
                    }
                    DownloadStatus::Verifying => {
                        if let Some(s) = spinner_bar.take() {
                            s.finish_and_clear();
                        }
                        let s = mp.add(create_spinner("Verifying SHA-256 integrity..."));
                        spinner_bar = Some(s);
                    }
                    DownloadStatus::Completed { .. } => {
                        if let Some(s) = spinner_bar.take() {
                            s.finish_and_clear();
                        }
                        if let Some(ob) = overall_bar.take() {
                            ob.finish_and_clear();
                        }
                        print_success("Download completed successfully. Integrity verified ✓");
                    }
                    DownloadStatus::Failed { error } => {
                        if let Some(s) = spinner_bar.take() {
                            s.finish_and_clear();
                        }
                        if let Some(ob) = overall_bar.take() {
                            ob.finish_and_clear();
                        }
                        for (_, cb) in chunk_bars.drain() {
                            cb.finish_and_clear();
                        }
                        print_error(&format!("Download failed: {}", error));
                    }
                }
            }

            if let Err(e) = download_handle.await? {
                print_error(&e.to_string());
            }
        }

        // ===================================================================
        // List
        // ===================================================================
        Commands::List { folder } => {
            let spinner = create_spinner(&format!("Listing files in '{}'...", folder));
            let files = match service.list_files(&folder).await {
                Ok(f) => {
                    spinner.finish_and_clear();
                    f
                }
                Err(e) => {
                    spinner.finish_and_clear();
                    print_error(&e.to_string());
                    return Ok(());
                }
            };

            if files.is_empty() {
                println!("No files found in '{}'", folder);
            } else {
                print_file_list(files);
            }
        }

        // ===================================================================
        // Rename
        // ===================================================================
        Commands::Rename { old_path, new_path } => {
            let spinner =
                create_spinner(&format!("Renaming '{}' to '{}'...", old_path, new_path));
            match service.rename_file(&old_path, &new_path).await {
                Ok(_) => {
                    spinner.finish_and_clear();
                    print_success(&format!("Renamed '{}' to '{}'", old_path, new_path));
                }
                Err(e) => {
                    spinner.finish_and_clear();
                    print_error(&format!("Rename failed: {}", e));
                }
            }
        }

        // ===================================================================
        // Delete
        // ===================================================================
        Commands::Delete { path } => {
            let spinner = create_spinner(&format!("Deleting '{}'...", path));
            match service.delete_file(&path).await {
                Ok(_) => {
                    spinner.finish_and_clear();
                    print_success(&format!(
                        "Deleted '{}' (Telegram & Metadata)",
                        path
                    ));
                }
                Err(e) => {
                    spinner.finish_and_clear();
                    print_error(&format!("Delete failed: {}", e));
                }
            }
        }
    }

    Ok(())
}
