use mongodb::{Client, Collection, options::ClientOptions};
use mongodb::bson::{doc, oid::ObjectId};
use crate::models::{FileMetadata, Bot};
use crate::errors::{Result, TgCloudError};
use futures::stream::TryStreamExt;

#[derive(Clone)]
pub struct MongoStore {
    client: Client,
    db_name: String,
}

impl MongoStore {
    pub async fn new(uri: &str) -> Result<Self> {
        let mut client_options = ClientOptions::parse(uri).await?;
        client_options.app_name = Some("tgcloud".to_string());
        let client = Client::with_options(client_options)?;

        Ok(Self {
            client,
            db_name: "tgcloud".to_string(),
        })
    }

    fn files_collection(&self) -> Collection<FileMetadata> {
        self.client.database(&self.db_name).collection("files")
    }

    fn bots_collection(&self) -> Collection<Bot> {
        self.client.database(&self.db_name).collection("bots")
    }

    // -----------------------------------------------------------------------
    // File CRUD
    // -----------------------------------------------------------------------

    pub async fn save_file(&self, file: FileMetadata) -> Result<ObjectId> {
        let result = self.files_collection().insert_one(file, None).await?;
        result
            .inserted_id
            .as_object_id()
            .ok_or_else(|| TgCloudError::Unknown("Failed to get inserted ID".to_string()))
    }

    pub async fn get_file_by_path(&self, path: &str) -> Result<Option<FileMetadata>> {
        self.files_collection()
            .find_one(doc! { "original_name": path }, None)
            .await
            .map_err(Into::into)
    }

    pub async fn get_file_by_id(&self, file_id: &str) -> Result<Option<FileMetadata>> {
        self.files_collection()
            .find_one(doc! { "file_id": file_id }, None)
            .await
            .map_err(Into::into)
    }

    pub async fn list_files(&self, folder_prefix: &str) -> Result<Vec<FileMetadata>> {
        let filter = if folder_prefix == "root" || folder_prefix.is_empty() {
            doc! {}
        } else {
            doc! { "original_name": { "$regex": format!("^{}", regex::escape(folder_prefix)) } }
        };

        let mut cursor = self.files_collection().find(filter, None).await?;
        let mut files = Vec::new();
        while let Some(file) = cursor.try_next().await? {
            files.push(file);
        }
        Ok(files)
    }

    pub async fn rename_file(&self, old_path: &str, new_path: &str) -> Result<()> {
        let count = self
            .files_collection()
            .count_documents(doc! { "original_name": new_path }, None)
            .await?;
        if count > 0 {
            return Err(TgCloudError::Unknown(format!(
                "File already exists at {}",
                new_path
            )));
        }

        let result = self
            .files_collection()
            .update_one(
                doc! { "original_name": old_path },
                doc! { "$set": { "original_name": new_path } },
                None,
            )
            .await?;

        if result.modified_count == 0 {
            return Err(TgCloudError::FileNotFound(old_path.to_string()));
        }
        Ok(())
    }

    pub async fn delete_file(&self, path: &str) -> Result<()> {
        let result = self
            .files_collection()
            .delete_one(doc! { "original_name": path }, None)
            .await?;
        if result.deleted_count == 0 {
            return Err(TgCloudError::FileNotFound(path.to_string()));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Bot CRUD
    // -----------------------------------------------------------------------

    pub async fn add_bot(&self, bot: Bot) -> Result<()> {
        let filter = doc! { "bot_id": bot.bot_id.clone() };
        self.bots_collection()
            .replace_one(
                filter,
                bot,
                mongodb::options::ReplaceOptions::builder()
                    .upsert(true)
                    .build(),
            )
            .await?;
        Ok(())
    }

    pub async fn get_active_bots(&self) -> Result<Vec<Bot>> {
        let mut cursor = self
            .bots_collection()
            .find(doc! { "active": true }, None)
            .await?;
        let mut bots = Vec::new();
        while let Some(bot) = cursor.try_next().await? {
            bots.push(bot);
        }
        Ok(bots)
    }

    pub async fn increment_bot_usage(&self, bot_id: &str) -> Result<()> {
        self.bots_collection()
            .update_one(
                doc! { "bot_id": bot_id },
                doc! { "$inc": { "upload_count": 1 } },
                None,
            )
            .await?;
        Ok(())
    }
}
