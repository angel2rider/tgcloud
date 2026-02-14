use crate::errors::{Result, TgCloudError};
use reqwest::{multipart, Body, Client, StatusCode};
use serde_json::Value;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio_util::codec::{BytesCodec, FramedRead};

/// Maximum number of retry attempts for transient errors.
const MAX_RETRIES: u32 = 5;
/// Base delay for exponential backoff.
const BASE_DELAY_MS: u64 = 1_000;

#[derive(Clone)]
pub struct TelegramClient {
    client: Client,
    api_url: String,
}

impl TelegramClient {
    /// Create a new client sharing the given `reqwest::Client`.
    pub fn with_client(client: Client, api_url: String) -> Self {
        Self { client, api_url }
    }

    pub fn new(api_url: String) -> Self {
        Self {
            client: Client::new(),
            api_url,
        }
    }

    /// Return a reference to the inner `reqwest::Client` so callers can share it.
    pub fn http_client(&self) -> &Client {
        &self.client
    }

    // -----------------------------------------------------------------------
    // Upload: full file (single chunk path)
    // -----------------------------------------------------------------------

    pub async fn upload_file(
        &self,
        token: &str,
        chat_id: &str,
        path: &str,
        _progress_callback: impl Fn(u64) + Send + Sync + 'static,
    ) -> Result<(String, i64)> {
        let file_path = std::path::Path::new(path);
        let file_name = file_path
            .file_name()
            .ok_or_else(|| TgCloudError::UploadFailed("Invalid file path".to_string()))?
            .to_string_lossy()
            .to_string();

        let token = token.to_string();
        let chat_id = chat_id.to_string();
        let api_url = self.api_url.clone();
        let client = self.client.clone();
        let path = path.to_string();

        self.with_retry(move || {
            let token = token.clone();
            let chat_id = chat_id.clone();
            let api_url = api_url.clone();
            let client = client.clone();
            let file_name = file_name.clone();
            let path = path.clone();
            async move {
                let file = tokio::fs::File::open(&path).await?;
                let stream = FramedRead::new(file, BytesCodec::new());
                let file_body = Body::wrap_stream(stream);
                upload_stream_inner(&client, &api_url, &token, &chat_id, file_name, file_body).await
            }
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Upload: streaming part (for chunked uploads)
    // -----------------------------------------------------------------------

    pub async fn upload_part(
        &self,
        token: &str,
        chat_id: &str,
        file_name: String,
        reader: impl tokio::io::AsyncRead + Send + Sync + 'static,
    ) -> Result<(String, i64)> {
        let stream = FramedRead::new(reader, BytesCodec::new());
        let file_body = Body::wrap_stream(stream);
        upload_stream_inner(
            &self.client,
            &self.api_url,
            token,
            chat_id,
            file_name,
            file_body,
        )
        .await
    }

    // -----------------------------------------------------------------------
    // Upload part with retry — re-opens the file for each attempt
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub async fn upload_part_with_retry(
        &self,
        token: &str,
        chat_id: &str,
        file_name: String,
        file_path: &str,
        offset: u64,
        length: u64,
        progress: Arc<AtomicU64>,
    ) -> Result<(String, i64)> {
        use tokio::io::AsyncSeekExt;

        let token = token.to_string();
        let chat_id = chat_id.to_string();
        let api_url = self.api_url.clone();
        let client = self.client.clone();
        let file_name_owned = file_name;
        let file_path_owned = file_path.to_string();

        self.with_retry(move || {
            let token = token.clone();
            let chat_id = chat_id.clone();
            let api_url = api_url.clone();
            let client = client.clone();
            let file_name = file_name_owned.clone();
            let file_path = file_path_owned.clone();
            let progress = Arc::clone(&progress);
            async move {
                let mut file = tokio::fs::File::open(&file_path).await?;
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                let reader = tokio::io::AsyncReadExt::take(file, length);
                let reader_with_progress = ProgressWrapper::new(reader, progress);
                let stream = FramedRead::new(reader_with_progress, BytesCodec::new());
                let file_body = Body::wrap_stream(stream);
                upload_stream_inner(&client, &api_url, &token, &chat_id, file_name, file_body).await
            }
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Delete message
    // -----------------------------------------------------------------------

    pub async fn delete_message(&self, token: &str, chat_id: &str, message_id: i64) -> Result<()> {
        let url = format!("{}/bot{}/deleteMessage", self.api_url, token);
        let params = [
            ("chat_id", chat_id.to_string()),
            ("message_id", message_id.to_string()),
        ];

        let res = self.client.post(&url).form(&params).send().await?;

        if !res.status().is_success() {
            return Err(TgCloudError::UploadFailed(format!(
                "Delete failed: {}",
                res.status()
            )));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Download helpers
    // -----------------------------------------------------------------------

    pub async fn get_download_url(&self, token: &str, file_id: &str) -> Result<String> {
        let url = format!("{}/bot{}/getFile?file_id={}", self.api_url, token, file_id);
        let res = self.client.get(&url).send().await?;
        let json: Value = res.json().await?;

        let file_path = json["result"]["file_path"].as_str().ok_or_else(|| {
            TgCloudError::DownloadFailed("No file_path in Telegram response".to_string())
        })?;

        Ok(format!("{}/file/bot{}/{}", self.api_url, token, file_path))
    }

    pub async fn download_file(&self, url: &str) -> Result<reqwest::Response> {
        let res = self.client.get(url).send().await?;
        if !res.status().is_success() {
            return Err(TgCloudError::DownloadFailed(format!(
                "Download failed: {}",
                res.status()
            )));
        }
        Ok(res)
    }

    /// Download with automatic retry on 429 / 5xx.
    pub async fn download_file_with_retry(
        &self,
        url: &str,
        _progress: Arc<AtomicU64>, // Kept for consistency, but we wrap the response stream chunking instead
    ) -> Result<reqwest::Response> {
        let client = self.client.clone();
        let url_owned = url.to_string();

        self.with_retry(move || {
            let client = client.clone();
            let url = url_owned.clone();
            async move {
                let res = client.get(&url).send().await?;
                check_transient_status(&res)?;
                if !res.status().is_success() {
                    return Err(TgCloudError::DownloadFailed(format!(
                        "Download failed: {}",
                        res.status()
                    )));
                }
                Ok(res)
            }
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Retry machinery
    // -----------------------------------------------------------------------

    /// Generic retry wrapper with exponential backoff + jitter.
    /// Retries on `RetryExhausted`-triggering transient errors; the closure
    /// must return our `Result<T>`.
    async fn with_retry<F, Fut, T>(&self, make_future: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut last_error = String::new();

        for attempt in 0..MAX_RETRIES {
            match make_future().await {
                Ok(val) => return Ok(val),
                Err(e) => {
                    if !is_retryable(&e) {
                        return Err(e);
                    }
                    last_error = e.to_string();
                    let delay = backoff_delay(attempt);
                    log::warn!(
                        "Retryable error (attempt {}/{}): {}. Retrying in {:?}",
                        attempt + 1,
                        MAX_RETRIES,
                        last_error,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Err(TgCloudError::RetryExhausted {
            attempts: MAX_RETRIES,
            last_error,
        })
    }
}

// ===========================================================================
// Free functions (not methods — avoids borrow issues with closures)
// ===========================================================================

async fn upload_stream_inner(
    client: &Client,
    api_url: &str,
    token: &str,
    chat_id: &str,
    file_name: String,
    body: Body,
) -> Result<(String, i64)> {
    let form = multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .part(
            "document",
            multipart::Part::stream(body).file_name(file_name),
        );

    let url = format!("{}/bot{}/sendDocument", api_url, token);
    let res = client.post(&url).multipart(form).send().await?;

    // Check for transient HTTP errors that should trigger retry.
    check_transient_status(&res)?;

    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await?;
        return Err(TgCloudError::UploadFailed(format!(
            "Telegram API error ({}): {}",
            status, text
        )));
    }

    let json: Value = res.json().await?;

    if !json["ok"].as_bool().unwrap_or(false) {
        return Err(TgCloudError::UploadFailed(format!(
            "Telegram API error: {}",
            json
        )));
    }

    let file_id = json["result"]["document"]["file_id"]
        .as_str()
        .ok_or_else(|| TgCloudError::UploadFailed("No file_id in response".to_string()))?
        .to_string();

    let message_id = json["result"]["message_id"]
        .as_i64()
        .ok_or_else(|| TgCloudError::UploadFailed("No message_id in response".to_string()))?;

    Ok((file_id, message_id))
}

/// Returns a retryable error if the response status is 429 or 5xx.
/// This must be called *before* consuming the response body.
fn check_transient_status(res: &reqwest::Response) -> Result<()> {
    let status = res.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(TgCloudError::UploadFailed(
            "Rate limited (HTTP 429)".to_string(),
        ));
    }
    if status.is_server_error() {
        return Err(TgCloudError::UploadFailed(format!(
            "Server error (HTTP {})",
            status.as_u16()
        )));
    }
    Ok(())
}

/// Determine whether an error is retryable (429 or 5xx related).
fn is_retryable(err: &TgCloudError) -> bool {
    match err {
        TgCloudError::UploadFailed(msg)
        | TgCloudError::DownloadFailed(msg)
        | TgCloudError::DeleteFailed(msg) => {
            msg.contains("429")
                || msg.contains("Rate limited")
                || msg.contains("Server error")
                || msg.contains("500")
                || msg.contains("502")
                || msg.contains("503")
                || msg.contains("504")
        }
        TgCloudError::RateLimited(_) => true,
        TgCloudError::TelegramError(_) => {
            // reqwest connection/timeout errors are retryable
            true
        }
        _ => false,
    }
}

/// Exponential backoff with jitter: base * 2^attempt ± 25%.
fn backoff_delay(attempt: u32) -> Duration {
    use rand::Rng;
    let base = BASE_DELAY_MS * 2u64.pow(attempt);
    let jitter_range = base / 4;
    let jitter: i64 = if jitter_range > 0 {
        rand::thread_rng().gen_range(-(jitter_range as i64)..=(jitter_range as i64))
    } else {
        0
    };
    let ms = (base as i64 + jitter).max(100) as u64;
    Duration::from_millis(ms)
}
/// A wrapper that tracks bytes read from an underlying AsyncRead.
struct ProgressWrapper<R> {
    inner: R,
    progress: Arc<AtomicU64>,
}

impl<R> ProgressWrapper<R> {
    fn new(inner: R, progress: Arc<AtomicU64>) -> Self {
        Self { inner, progress }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ProgressWrapper<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len() - before;
                self.progress.fetch_add(n as u64, Ordering::Relaxed);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}
