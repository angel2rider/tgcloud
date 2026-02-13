use reqwest::{Client, Body, multipart};
use tokio::fs::File;
use tokio_util::codec::{BytesCodec, FramedRead};
use crate::errors::{Result, TgCloudError};
use serde_json::Value;

#[derive(Clone)]
pub struct TelegramClient {
    client: Client,
    api_url: String,
}

impl TelegramClient {
    pub fn new(api_url: String) -> Self {
        Self {
            client: Client::new(),
            api_url,
        }
    }

    pub async fn upload_file(
        &self, 
        token: &str, 
        chat_id: &str, 
        path: &str, 
        _progress_callback: impl Fn(u64) + Send + Sync + 'static
    ) -> Result<(String, i64)> {
        let file_path = std::path::Path::new(path);
        let file_name = file_path.file_name()
            .ok_or_else(|| TgCloudError::UploadFailed("Invalid file path".to_string()))?
            .to_string_lossy()
            .to_string();

        let file = File::open(path).await?;
        let stream = FramedRead::new(file, BytesCodec::new());
        let file_body = Body::wrap_stream(stream);

        self.upload_stream(token, chat_id, file_name, file_body).await
    }

    pub async fn upload_part(
        &self,
        token: &str,
        chat_id: &str,
        file_name: String,
        reader: impl tokio::io::AsyncRead + Send + Sync + 'static,
    ) -> Result<(String, i64)> {
        let stream = FramedRead::new(reader, BytesCodec::new());
        let file_body = Body::wrap_stream(stream);
        self.upload_stream(token, chat_id, file_name, file_body).await
    }

    async fn upload_stream(
        &self,
        token: &str,
        chat_id: &str,
        file_name: String,
        body: Body,
    ) -> Result<(String, i64)> {
        let form = multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", multipart::Part::stream(body).file_name(file_name));

        let url = format!("{}/bot{}/sendDocument", self.api_url, token);
        let res = self.client.post(&url)
            .multipart(form)
            .send()
            .await?;

        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await?;
            return Err(TgCloudError::UploadFailed(format!("Telegram API error ({}): {}", status, text)));
        }
        
        let json: Value = res.json().await?;
        
        if !json["ok"].as_bool().unwrap_or(false) {
             return Err(TgCloudError::UploadFailed(format!("Telegram API error: {}", json)));
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

    pub async fn delete_message(&self, token: &str, chat_id: &str, message_id: i64) -> Result<()> {
        let url = format!("{}/bot{}/deleteMessage", self.api_url, token);
        let params = [("chat_id", chat_id), ("message_id", &message_id.to_string())];
        
        let res = self.client.post(&url)
            .form(&params)
            .send()
            .await?;

        if !res.status().is_success() {
             // We return error but maybe delete failed because message gone? 
             // We treat any failure as error for now.
             return Err(TgCloudError::UploadFailed(format!("Delete failed: {}", res.status())));
        }
        Ok(())
    }

    pub async fn get_download_url(&self, token: &str, file_id: &str) -> Result<String> {
        let url = format!("{}/bot{}/getFile?file_id={}", self.api_url, token, file_id);
        let res = self.client.get(&url).send().await?;
        let json: Value = res.json().await?;
        
        let file_path = json["result"]["file_path"]
            .as_str()
            .ok_or_else(|| TgCloudError::UploadFailed("No file_path in response".to_string()))?;

        Ok(format!("{}/file/bot{}/{}", self.api_url, token, file_path))
    }
    
    pub async fn download_file(&self, url: &str) -> Result<reqwest::Response> {
        let res = self.client.get(url).send().await?;
        if !res.status().is_success() {
             return Err(TgCloudError::UploadFailed(format!("Download failed: {}", res.status())));
        }
        Ok(res)
    }
}
