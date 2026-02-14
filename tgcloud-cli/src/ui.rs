use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use console::{style, Emoji};
use comfy_table::{Table, presets::UTF8_FULL, Cell, Color, Attribute};
use std::time::Duration;
use human_bytes::human_bytes;

// ---------------------------------------------------------------------------
// Banner & messages
// ---------------------------------------------------------------------------

pub fn print_banner() {
    println!();
    println!("{}", style("  TGCloud  ").bold().white().on_blue());
    println!("{}", style("  Telegram-backed distributed storage  ").dim());
    println!();
}

pub fn print_success(message: &str) {
    println!("{} {}", Emoji("✅", "OK"), style(message).green());
}

pub fn print_error(message: &str) {
    eprintln!("{} {}", Emoji("❌", "Error"), style(message).red());
}

// ---------------------------------------------------------------------------
// Spinners
// ---------------------------------------------------------------------------

pub fn create_spinner(message: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
            .template("{spinner:.blue} {msg}")
            .expect("invalid spinner template"),
    );
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

// ---------------------------------------------------------------------------
// Multi-progress helpers
// ---------------------------------------------------------------------------

pub fn create_multi_progress() -> MultiProgress {
    MultiProgress::new()
}

/// Overall file progress bar (tracks total bytes across all chunks).
pub fn create_overall_bar(mp: &MultiProgress, total_size: u64) -> ProgressBar {
    let pb = mp.add(ProgressBar::new(total_size));
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} Overall  [{elapsed_precise}] [{bar:40.cyan/blue}] \
                 {bytes}/{total_bytes} ({bytes_per_sec}) ETA {eta}",
            )
            .expect("invalid bar template")
            .progress_chars("█▓░"),
    );
    pb.enable_steady_tick(Duration::from_millis(200));
    pb
}

/// Per-chunk progress bar.
pub fn create_chunk_bar(
    mp: &MultiProgress,
    chunk_index: u32,
    total_chunks: u32,
    chunk_size: u64,
) -> ProgressBar {
    let pb = mp.add(ProgressBar::new(chunk_size));
    pb.set_style(
        ProgressStyle::default_bar()
            .template(&format!(
                "  {{spinner:.blue}} Chunk {}/{} [{{bar:30.magenta/blue}}] \
                 {{bytes}}/{{total_bytes}} ({{bytes_per_sec}}) ETA {{eta}}",
                chunk_index, total_chunks
            ))
            .expect("invalid chunk bar template")
            .progress_chars("█▓░"),
    );
    pb.enable_steady_tick(Duration::from_millis(200));
    pb
}


// ---------------------------------------------------------------------------
// File listing table
// ---------------------------------------------------------------------------

pub fn print_file_list(files: Vec<tgcloud_core::FileMetadata>) {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);

    table.set_header(vec![
        Cell::new("Name")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
        Cell::new("Size")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Chunks")
            .add_attribute(Attribute::Bold)
            .fg(Color::Yellow),
        Cell::new("Created At")
            .add_attribute(Attribute::Bold)
            .fg(Color::Yellow),
        Cell::new("File ID")
            .add_attribute(Attribute::Bold)
            .fg(Color::Magenta),
    ]);

    for file in files {
        table.add_row(vec![
            Cell::new(&file.original_name),
            Cell::new(human_bytes(file.size as f64)),
            Cell::new(format!("{}", file.total_chunks)),
            Cell::new(file.created_at.to_rfc3339()),
            Cell::new(&file.file_id),
        ]);
    }

    println!("{table}");
}
