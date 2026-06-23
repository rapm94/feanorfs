use anyhow::{bail, Context, Result};
use reqwest::Client;
use fs_sync_common::{SyncRequest, SyncResponse};

pub struct ApiClient {
    client: Client,
    server_url: String,
}

impl ApiClient {
    pub fn new(server_url: &str) -> Self {
        Self {
            client: Client::new(),
            server_url: server_url.trim_end_matches('/').to_string(),
        }
    }

    pub async fn negotiate_sync(&self, request: &SyncRequest) -> Result<SyncResponse> {
        let url = format!("{}/api/sync/diff", self.server_url);
        let resp = self.client
            .post(&url)
            .json(request)
            .send()
            .await
            .context("Failed to send sync diff request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Sync negotiation failed with status {}: {}", status, body);
        }

        let response: SyncResponse = resp.json()
            .await
            .context("Failed to parse sync diff response")?;

        Ok(response)
    }

    pub async fn upload_file(
        &self,
        workspace_id: &str,
        path: &str,
        hash: &str,
        size: u64,
        mtime: i64,
        content: Vec<u8>,
    ) -> Result<()> {
        let url = format!("{}/api/upload", self.server_url);
        
        let resp = self.client
            .post(&url)
            .query(&[
                ("workspace_id", workspace_id),
                ("path", path),
                ("hash", hash),
                ("size", &size.to_string()),
                ("mtime", &mtime.to_string()),
            ])
            .body(content)
            .send()
            .await
            .context("Failed to send upload request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Upload failed with status {}: {}", status, body);
        }

        Ok(())
    }

    pub async fn download_file(&self, hash: &str) -> Result<Vec<u8>> {
        let url = format!("{}/api/download/{}", self.server_url, hash);
        
        let resp = self.client
            .get(&url)
            .send()
            .await
            .context("Failed to send download request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Download failed with status {}: {}", status, body);
        }

        let bytes = resp.bytes()
            .await
            .context("Failed to read download body bytes")?;

        Ok(bytes.to_vec())
    }
}

