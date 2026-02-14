mod ui;

use anyhow::Context;
use clap::{Parser, Subcommand};
use dotenv::dotenv;
use indicatif::ProgressBar;
use owo_colors::OwoColorize;
use std::env;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tgcloud_core::{BotConfig, Config, DownloadStatus, TgCloudService, UploadStatus};
use tokio::sync::mpsc;
use ui::*;

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
    Rename { old_path: String, new_path: String },
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
    let telegram_chat_id = env::var("TELEGRAM_CHAT_ID").context("TELEGRAM_CHAT_ID must be set")?;

    let mut bots = Vec::new();
    if let Ok(bots_json) = env::var("BOTS_JSON") {
        bots = serde_json::from_str(&bots_json).context("Failed to parse BOTS_JSON")?;
    } else if let (Ok(id), Ok(token)) = (env::var("BOT_ID"), env::var("BOT_TOKEN")) {
        bots.push(BotConfig { bot_id: id, token });
    }

    let config = Config {
        mongo_uri,
        telegram_api_url,
        telegram_chat_id,
        bots,
        max_global_concurrency: tgcloud_core::config::DEFAULT_MAX_GLOBAL_CONCURRENCY,
        max_per_bot_concurrency: tgcloud_core::config::DEFAULT_MAX_PER_BOT_CONCURRENCY,
    };

    let spinner = create_spinner("Connecting to services...");
    let service = TgCloudService::new(config)
        .await
        .inspect_err(|_| {
            spinner.finish_and_clear();
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
            let upload_handle =
                tokio::spawn(async move { service_handle.upload_file(&path, tx).await });

            let mut progress_bar: Option<ProgressBar> = None;
            let mut spinner: Option<ProgressBar> = None;

            while let Some(event) = rx.recv().await {
                match event.status {
                    UploadStatus::Started {
                        total_size,
                        total_chunks: _,
                        progress,
                    } => {
                        if total_size > 256 * 1024 * 1024 {
                            let pb = create_overall_bar_direct(total_size);
                            progress_bar = Some(pb.clone());

                            // Spawn a task to update the progress bar from the atomic counter.
                            tokio::spawn(async move {
                                while !pb.is_finished() {
                                    let current = progress.load(Ordering::Relaxed);
                                    pb.set_position(current);
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                }
                            });
                        } else {
                            spinner = Some(create_spinner("Uploading..."));
                        }
                    }
                    UploadStatus::Hashing => {
                        if let Some(s) = spinner.take() {
                            s.finish_and_clear();
                        }
                        spinner = Some(create_spinner("Calculating SHA-256 hash..."));
                    }
                    UploadStatus::HashComplete { sha256 } => {
                        if let Some(s) = spinner.take() {
                            s.finish_and_clear();
                        }
                        println!(
                            "  {} SHA-256: {}",
                            "🔒".cyan(),
                            sha256[..16].to_string().yellow()
                        );
                    }
                    UploadStatus::Completed { file_id } => {
                        if let Some(pb) = progress_bar.take() {
                            pb.finish_and_clear();
                        }
                        if let Some(s) = spinner.take() {
                            s.finish_and_clear();
                        }
                        print_success(&format!("Upload completed!\n    File ID: {}\n", file_id));
                    }
                    UploadStatus::Failed { error } => {
                        if let Some(pb) = progress_bar.take() {
                            pb.finish_and_clear();
                        }
                        if let Some(s) = spinner.take() {
                            s.finish_and_clear();
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

            let mut progress_bar: Option<ProgressBar> = None;
            let mut spinner: Option<ProgressBar> = None;

            while let Some(event) = rx.recv().await {
                match event.status {
                    DownloadStatus::Started {
                        total_size,
                        total_chunks,
                        progress,
                    } => {
                        println!(
                            "  {} File: {} in {} chunk(s)",
                            "📁".cyan(),
                            human_bytes::human_bytes(total_size as f64).yellow(),
                            total_chunks.to_string().green()
                        );

                        if total_size > 256 * 1024 * 1024 {
                            let pb = create_overall_bar_direct(total_size);
                            progress_bar = Some(pb.clone());

                            // Spawn a task to update the progress bar from the atomic counter.
                            tokio::spawn(async move {
                                while !pb.is_finished() {
                                    let current = progress.load(Ordering::Relaxed);
                                    pb.set_position(current);
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                }
                            });
                        } else {
                            spinner = Some(create_spinner("Downloading..."));
                        }
                    }
                    DownloadStatus::Merging => {
                        if let Some(pb) = progress_bar.take() {
                            pb.finish_and_clear();
                        }
                        if let Some(s) = spinner.take() {
                            s.finish_and_clear();
                        }
                        spinner = Some(create_spinner("Merging chunks..."));
                    }
                    DownloadStatus::Verifying => {
                        if let Some(s) = spinner.take() {
                            s.finish_and_clear();
                        }
                        spinner = Some(create_spinner("Verifying SHA-256 integrity..."));
                    }
                    DownloadStatus::Completed { .. } => {
                        if let Some(s) = spinner.take() {
                            s.finish_and_clear();
                        }
                        print_success("Download completed successfully. Integrity verified ✓");
                    }
                    DownloadStatus::Failed { error } => {
                        if let Some(pb) = progress_bar.take() {
                            pb.finish_and_clear();
                        }
                        if let Some(s) = spinner.take() {
                            s.finish_and_clear();
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
            let spinner = create_spinner(&format!("Renaming '{}' to '{}'...", old_path, new_path));
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
                    print_success(&format!("Deleted '{}' (Telegram & Metadata)", path));
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
