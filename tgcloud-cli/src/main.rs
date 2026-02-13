mod ui;

use clap::{Parser, Subcommand};
use dotenv::dotenv;
use std::env;
use tgcloud_core::{TgCloudService, UploadStatus, DownloadStatus, Config, BotConfig};
use tokio::sync::mpsc;
use anyhow::Context;
use ui::*;
use owo_colors::OwoColorize;

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
    Upload {
        path: String,
    },
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
    Delete {
        path: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    // env_logger::init(); // Replaced by tracing if needed, but for CLI UI we keep it clean unless --debug

    let args = Cli::parse();
    
    print_banner();

    // Load configuration
    let mongo_uri = env::var("MONGO_URI").context("MONGO_URI must be set")?;
    let telegram_api_url = env::var("TELEGRAM_API_URL").unwrap_or_else(|_| "http://localhost:8081".to_string());
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
    };

    let spinner = create_spinner("Connecting to services...");
    let service = TgCloudService::new(config).await
        .map_err(|e| {
            spinner.finish_and_clear(); 
            e
        })
        .context("Failed to initialize service")?;
    spinner.finish_and_clear();

    match args.command {
        Commands::Upload { path } => {
            println!("🚀 Starting upload for: {}", path.cyan());
            let (tx, mut rx) = mpsc::channel(100);
            
            let service_handle = service;
            let upload_handle = tokio::spawn(async move {
                service_handle.upload_file(&path, tx).await
            });

            let mut pb = None;

            while let Some(event) = rx.recv().await {
                match event.status {
                    UploadStatus::Started => {
                        // We don't know total size here immediately unless we stat file first or wait for progress
                        // service emits hash calc start maybe?
                        // For now, wait for progress to init bar or just spinner
                        let s = create_spinner("Preparing upload...");
                        pb = Some(s);
                    }
                    UploadStatus::Progress { sent, total } => {
                        if let Some(bar) = &pb {
                            if bar.length() == Some(u64::MAX) || bar.length().is_none() {
                                // Switch to upload bar
                                bar.finish_and_clear();
                                pb = Some(create_upload_pb(total));
                            }
                        } else {
                             pb = Some(create_upload_pb(total));
                        }
                        
                        if let Some(bar) = &pb {
                            bar.set_position(sent);
                        }
                    }
                    UploadStatus::Completed { file_id } => {
                        if let Some(bar) = &pb {
                            bar.finish_and_clear();
                        }
                        print_success(&format!("Upload completed!\n    ID: {}\n", file_id));
                    }
                    UploadStatus::Failed { error } => {
                        if let Some(bar) = &pb {
                            bar.finish_and_clear();
                        }
                        print_error(&format!("Upload failed: {}", error));
                    }
                }
            }

            if let Err(e) = upload_handle.await? {
                print_error(&e.to_string());
            }
        }
        Commands::Download { remote_path, local_path } => {
            println!("📥 Downloading {} to {}", remote_path.cyan(), local_path.green());
            
            let (tx, mut rx) = mpsc::channel(100);
            let service_handle = service;
            
            let download_handle = tokio::spawn(async move {
                service_handle.download_file(&remote_path, &local_path, tx).await
            });

            let mut pb = None;

            while let Some(event) = rx.recv().await {
                 match event.status {
                    DownloadStatus::Started { total_size } => {
                        pb = Some(create_download_pb(total_size));
                    }
                    DownloadStatus::Progress { downloaded, total } => {
                        if let Some(bar) = &pb {
                             bar.set_position(downloaded);
                             if bar.length() != Some(total) {
                                 bar.set_length(total);
                             }
                        }
                    }
                    DownloadStatus::Completed { .. } => {
                        if let Some(bar) = &pb {
                            bar.finish_and_clear();
                        }
                        print_success("Download completed successfully.");
                    }
                    DownloadStatus::Failed { error } => {
                        if let Some(bar) = &pb {
                            bar.finish_and_clear();
                        }
                        print_error(&format!("Download failed: {}", error));
                    }
                 }
            }
            
            if let Err(e) = download_handle.await? {
                 print_error(&e.to_string());
            }
        }
        Commands::List { folder } => {
            let spinner = create_spinner(&format!("Listing files in '{}'...", folder));
            let files = match service.list_files(&folder).await {
                Ok(f) => {
                    spinner.finish_and_clear();
                    f
                },
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
        Commands::Delete { path } => {
            // Confirm delete? No, requirement says "hard delete", usually implies force or just do it. 
            // We'll just do it for now as per requirements "tgcloud delete <path>".
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
