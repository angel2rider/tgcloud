use indicatif::{ProgressBar, ProgressStyle};
use console::{style, Emoji};
use comfy_table::{Table, presets::UTF8_FULL, Cell, Color, Attribute};
use std::time::Duration;
use human_bytes::human_bytes;


pub fn print_banner() {
    println!();
    println!("{}", style("  TGCloud  ").bold().white().on_blue());
    println!("{}", style("  Telegram-backed distributed storage  ").dim());
    println!();
}

pub fn create_spinner(message: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::default_spinner()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
        .template("{spinner:.blue} {msg}")
        .unwrap());
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

pub fn create_upload_pb(total_size: u64) -> ProgressBar {
    let pb = ProgressBar::new(total_size);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta}) {msg}")
        .unwrap()
        .progress_chars("#>-"));
    pb
}

pub fn create_download_pb(total_size: u64) -> ProgressBar {
    let pb = ProgressBar::new(total_size);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.magenta/blue}] {bytes}/{total_bytes} ({eta}) {msg}")
        .unwrap()
        .progress_chars("#>-"));
    pb
}

pub fn print_success(message: &str) {
    println!("{} {}", Emoji("✅", "OK"), style(message).green());
}

pub fn print_error(message: &str) {
    eprintln!("{} {}", Emoji("❌", "Error"), style(message).red());
}

pub fn print_file_list(files: Vec<tgcloud_core::File>) {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
    
    table.set_header(vec![
        Cell::new("Path").add_attribute(Attribute::Bold).fg(Color::Cyan),
        Cell::new("Size").add_attribute(Attribute::Bold).fg(Color::Green),
        Cell::new("Created At").add_attribute(Attribute::Bold).fg(Color::Yellow),
        Cell::new("Bot ID").add_attribute(Attribute::Bold).fg(Color::Magenta),
    ]);

    for file in files {
        table.add_row(vec![
            Cell::new(file.path),
            Cell::new(human_bytes(file.size as f64)),
            Cell::new(file.created_at.to_rfc3339()),
            Cell::new(file.bot_id),
        ]);
    }

    println!("{table}");
}
