use anyhow::{bail, Context, Result};
use feanorfs_common::{SyncRequest, SyncResponse};
use reqwest::Client;

pub struct ApiClient {
    client: Client,
    server_url: String,
    server_password: Option<String>,
}

impl ApiClient {
    pub fn new(server_url: &str, server_password: Option<&str>) -> Self {
        Self {
            client: Client::new(),
            server_url: server_url.trim_end_matches('/').to_string(),
            server_password: server_password.map(|s| s.to_string()),
        }
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.server_password {
            Some(pass) => req.bearer_auth(pass),
            None => req,
        }
    }

    async fn post_sync_endpoint(
        &self,
        endpoint: &str,
        request: &SyncRequest,
    ) -> Result<SyncResponse> {
        let url = format!("{}/api/sync/{}", self.server_url, endpoint);
        let resp = self
            .authed(self.client.post(&url).json(request))
            .send()
            .await
            .with_context(|| format!("Failed to send sync {endpoint} request"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Server requires a password. Run 'feanorfs connect <URL> --token <PASS>'");
            }
            let body = resp.text().await.unwrap_or_default();
            bail!("Sync {endpoint} failed with status {status}: {body}");
        }

        resp.json()
            .await
            .with_context(|| format!("Failed to parse sync {endpoint} response"))
    }

    pub async fn peek_sync(&self, request: &SyncRequest) -> Result<SyncResponse> {
        self.post_sync_endpoint("peek", request).await
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

        let resp = self
            .authed(
                self.client
                    .post(&url)
                    .query(&[
                        ("workspace_id", workspace_id),
                        ("path", path),
                        ("hash", hash),
                        ("size", &size.to_string()),
                        ("mtime", &mtime.to_string()),
                        ("deleted", "false"),
                    ])
                    .body(content),
            )
            .send()
            .await
            .context("Failed to send upload request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Server requires a password. Run 'feanorfs connect <URL> --token <PASS>'");
            }
            let body = resp.text().await.unwrap_or_default();
            bail!("Upload failed with status {status}: {body}");
        }

        Ok(())
    }

    pub async fn upload_tombstone(
        &self,
        workspace_id: &str,
        path: &str,
        hash: &str,
        mtime: i64,
    ) -> Result<()> {
        let url = format!("{}/api/upload", self.server_url);
        let resp = self
            .authed(self.client.post(&url).query(&[
                ("workspace_id", workspace_id),
                ("path", path),
                ("hash", hash),
                ("size", "0"),
                ("mtime", &mtime.to_string()),
                ("deleted", "true"),
            ]))
            .send()
            .await
            .context("Failed to send tombstone upload")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Tombstone upload failed with status {status}: {body}");
        }
        Ok(())
    }

    pub async fn download_file(&self, hash: &str) -> Result<Vec<u8>> {
        let url = format!("{}/api/download/{}", self.server_url, hash);

        let resp = self
            .authed(self.client.get(&url))
            .send()
            .await
            .context("Failed to send download request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Server requires a password. Run 'feanorfs connect <URL> --token <PASS>'");
            }
            let body = resp.text().await.unwrap_or_default();
            bail!("Download failed with status {status}: {body}");
        }

        let bytes = resp
            .bytes()
            .await
            .context("Failed to read download body bytes")?;

        Ok(bytes.to_vec())
    }

    pub async fn get_workspaces(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/workspaces", self.server_url);
        let resp = self
            .authed(self.client.get(&url))
            .send()
            .await
            .context("Failed to send workspaces request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Server requires a password. Run 'feanorfs connect <URL> --token <PASS>'");
            }
            let body = resp.text().await.unwrap_or_default();
            bail!("Fetch workspaces failed with status {status}: {body}");
        }

        let workspaces: Vec<String> = resp
            .json()
            .await
            .context("Failed to parse workspaces response")?;

        Ok(workspaces)
    }
}
