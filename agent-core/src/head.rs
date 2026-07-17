use crate::api::ApiClient;
use anyhow::{bail, Context, Result};
use feanorfs_common::{HeadResponse, SwapHeadRequest};

/// Outcome of an opaque workspace-head compare-and-swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapHeadResult {
    Swapped,
    Conflict(Option<String>),
}

impl ApiClient {
    /// Reads the current opaque snapshot id for a workspace.
    ///
    /// # Errors
    /// Returns an error for transport, authorization, status, or JSON failures.
    pub async fn get_head(&self, workspace_id: &str) -> Result<Option<String>> {
        let query = format!("workspace_id={}", urlencoding::encode(workspace_id));
        let (status, bytes) = self
            .raw_request(http::Method::GET, "/api/head", &query, Vec::new(), None)
            .await?;
        ensure_authorized(status)?;
        if !status.is_success() {
            bail!(
                "GET /api/head failed with status {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        let response: HeadResponse =
            serde_json::from_slice(&bytes).context("parse workspace head response")?;
        Ok(response.snapshot_id)
    }

    /// Atomically replaces a workspace head when `expected` still matches.
    ///
    /// # Errors
    /// Returns an error for transport, authorization, unexpected status, or JSON failures.
    pub async fn swap_head(
        &self,
        workspace_id: &str,
        expected: Option<&str>,
        new: &str,
    ) -> Result<SwapHeadResult> {
        let request = SwapHeadRequest {
            workspace_id: workspace_id.to_string(),
            expected: expected.map(str::to_string),
            new: new.to_string(),
        };
        let body = serde_json::to_vec(&request).context("serialize head swap request")?;
        let (status, bytes) = self
            .raw_request(
                http::Method::PUT,
                "/api/head",
                "",
                body,
                Some("application/json"),
            )
            .await?;
        ensure_authorized(status)?;
        match status {
            http::StatusCode::OK => Ok(SwapHeadResult::Swapped),
            http::StatusCode::CONFLICT => {
                let response: HeadResponse =
                    serde_json::from_slice(&bytes).context("parse head swap conflict response")?;
                Ok(SwapHeadResult::Conflict(response.snapshot_id))
            }
            other => bail!(
                "PUT /api/head failed with status {other}: {}",
                String::from_utf8_lossy(&bytes)
            ),
        }
    }
}

fn ensure_authorized(status: http::StatusCode) -> Result<()> {
    if status == http::StatusCode::UNAUTHORIZED {
        bail!("Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`");
    }
    Ok(())
}
