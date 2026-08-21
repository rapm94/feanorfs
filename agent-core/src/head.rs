use crate::api::{request_status_error, ApiClient};
use anyhow::{bail, Context, Result};
use feanorfs_common::{HeadResponse, SwapHeadRequest};
use std::time::Duration;

/// Hard cap for one requested head wait; mirrors the hub-side bound and stays
/// below the 60-second HTTP read-idle timeout.
pub const MAX_HEAD_WAIT_MS: u64 = 30_000;

/// Minimum interval between compatibility-fallback head polls. Prevents an
/// old hub (or an unsupported wait) from ever producing a tight request loop.
const MIN_FALLBACK_POLL: Duration = Duration::from_secs(2);

/// Maximum compatibility-fallback poll interval.
const MAX_FALLBACK_POLL: Duration = Duration::from_secs(10);

/// Outcome of one bounded head-change observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadWaitOutcome {
    /// The head differs from `after`; carries the new head.
    Changed(Option<String>),
    /// The wait elapsed without a change; carries the unchanged head.
    Unchanged(Option<String>),
    /// The hub ignored the wait parameters (old hub); carries the current
    /// head. Callers use bounded polling with jitter instead.
    Unsupported(Option<String>),
}

/// Bounded wait for the opaque workspace head to change.
///
/// Uses the hub's extended `GET /api/head` wait semantics when supported and
/// reports [`HeadWaitOutcome::Unsupported`] when an old hub ignores the
/// parameters, so callers never fall into a busy loop. The future can be
/// dropped or raced with cancellation at any time; the hub releases waiter
/// capacity on disconnect.
///
/// # Errors
/// Returns an error for transport, authorization, status, or JSON failures.
pub async fn wait_for_head_change(
    api: &ApiClient,
    workspace_id: &str,
    after: Option<&str>,
    wait: Duration,
) -> Result<HeadWaitOutcome> {
    let wait_ms = wait.as_millis().min(MAX_HEAD_WAIT_MS as u128) as u64;
    let mut query = format!("workspace_id={}", urlencoding::encode(workspace_id));
    if let Some(after) = after {
        query.push_str("&after=");
        query.push_str(&urlencoding::encode(after));
    }
    query.push_str(&format!("&wait_ms={wait_ms}"));
    let (status, bytes) = api
        .raw_request(http::Method::GET, "/api/head", &query, Vec::new(), None)
        .await?;
    ensure_authorized(status)?;
    if !status.is_success() {
        return Err(request_status_error("GET", "/api/head", status, &bytes));
    }
    let response: HeadResponse =
        serde_json::from_slice(&bytes).context("parse workspace head response")?;
    if !response.wait_supported {
        return Ok(HeadWaitOutcome::Unsupported(response.snapshot_id));
    }
    if response.snapshot_id == after.map(str::to_string) {
        Ok(HeadWaitOutcome::Unchanged(response.snapshot_id))
    } else {
        Ok(HeadWaitOutcome::Changed(response.snapshot_id))
    }
}

/// Result of one observation window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadObservation {
    /// Current head after the window.
    pub head: Option<String>,
    /// True when the head changed during the window.
    pub changed: bool,
    /// True when the hub lacks wait support; callers retain their periodic
    /// pass as the recovery backstop.
    pub unsupported: bool,
}

/// Reusable bounded head-observation contract shared by the workspace
/// watcher, the events loop, the agent runner, and the continuous controller.
///
/// On hubs with wait support a call spends at most `window` in one long
/// request. Old hubs and retryable transport failures use bounded polling
/// within the same window.
pub struct HeadObserver<'a> {
    api: ObserverApi<'a>,
    workspace_id: String,
    known: Option<String>,
    wait_supported: Option<bool>,
    consecutive_failures: u32,
}

enum ObserverApi<'a> {
    Borrowed(&'a ApiClient),
    Owned(std::sync::Arc<ApiClient>),
}

impl ObserverApi<'_> {
    fn client(&self) -> &ApiClient {
        match self {
            ObserverApi::Borrowed(client) => client,
            ObserverApi::Owned(client) => client,
        }
    }
}

impl<'a> HeadObserver<'a> {
    pub fn new(api: &'a ApiClient, workspace_id: &str) -> Self {
        Self::with_api(ObserverApi::Borrowed(api), workspace_id)
    }

    /// Owns an `Arc<ApiClient>` so the observer can move into a spawned task.
    pub fn new_owned(api: std::sync::Arc<ApiClient>, workspace_id: &str) -> Self {
        Self::with_api(ObserverApi::Owned(api), workspace_id)
    }

    fn with_api(api: ObserverApi<'a>, workspace_id: &str) -> Self {
        Self {
            api,
            workspace_id: workspace_id.to_string(),
            known: None,
            wait_supported: None,
            consecutive_failures: 0,
        }
    }

    pub fn known(&self) -> Option<&str> {
        self.known.as_deref()
    }

    /// Acknowledges `head` as already observed without waiting.
    pub fn acknowledge(&mut self, head: Option<String>) {
        self.known = head;
    }

    /// Observes for up to `window`, returning whether the head changed.
    ///
    /// # Errors
    /// Returns an error only for fail-closed conditions (authorization,
    /// malformed responses, corrupt state). Retryable transport failures and
    /// unsupported hubs degrade to bounded polling inside the window.
    pub async fn observe(&mut self, window: Duration) -> Result<HeadObservation> {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(HeadObservation {
                    head: self.known.clone(),
                    changed: false,
                    unsupported: self.wait_supported == Some(false),
                });
            }
            let supported = self.wait_supported.unwrap_or(true);
            if supported {
                let request = wait_for_head_change(
                    self.api.client(),
                    &self.workspace_id,
                    self.known.as_deref(),
                    remaining,
                );
                match tokio::time::timeout(remaining, request).await {
                    Ok(Ok(HeadWaitOutcome::Changed(head))) => {
                        self.consecutive_failures = 0;
                        self.wait_supported = Some(true);
                        self.known = head.clone();
                        return Ok(HeadObservation {
                            head,
                            changed: true,
                            unsupported: false,
                        });
                    }
                    Ok(Ok(HeadWaitOutcome::Unchanged(head))) => {
                        self.consecutive_failures = 0;
                        self.wait_supported = Some(true);
                        if head != self.known {
                            // The hub returned a different head than we asked
                            // about; treat it as an observed change.
                            self.known = head.clone();
                            return Ok(HeadObservation {
                                head,
                                changed: true,
                                unsupported: false,
                            });
                        }
                        return Ok(HeadObservation {
                            head,
                            changed: false,
                            unsupported: false,
                        });
                    }
                    Ok(Ok(HeadWaitOutcome::Unsupported(head))) => {
                        self.wait_supported = Some(false);
                        self.consecutive_failures = 0;
                        if head != self.known {
                            self.known = head.clone();
                            return Ok(HeadObservation {
                                head,
                                changed: true,
                                unsupported: false,
                            });
                        }
                    }
                    Ok(Err(error)) if crate::api::is_retryable_transport_error(&error) => {
                        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                    }
                }
            } else {
                let request = self.api.client().get_head(&self.workspace_id);
                match tokio::time::timeout(remaining, request).await {
                    Ok(Ok(head)) => {
                        self.consecutive_failures = 0;
                        if head != self.known {
                            self.known = head.clone();
                            return Ok(HeadObservation {
                                head,
                                changed: true,
                                unsupported: true,
                            });
                        }
                    }
                    Ok(Err(error)) if crate::api::is_retryable_transport_error(&error) => {
                        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(self.fallback_delay().min(remaining)).await;
        }
    }

    fn fallback_delay(&self) -> Duration {
        let factor = 1u32 << self.consecutive_failures.min(6);
        let base = MIN_FALLBACK_POLL.saturating_mul(factor);
        // Jitter within [base/2, base] keeps independent observers from
        // synchronizing their fallback polls.
        let half = base / 2;
        let jittered = half
            + std::time::Duration::from_millis(
                chrono::Utc::now().timestamp_millis().rem_euclid(1000) as u64
                    * base.as_millis() as u64
                    / 2000,
            );
        jittered.min(MAX_FALLBACK_POLL)
    }
}

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
            return Err(request_status_error("GET", "/api/head", status, &bytes));
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
            other => Err(request_status_error("PUT", "/api/head", other, &bytes)),
        }
    }
}

fn ensure_authorized(status: http::StatusCode) -> Result<()> {
    if status == http::StatusCode::UNAUTHORIZED {
        bail!(
            "Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn serve(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{address}"), task)
    }

    /// An old hub that ignores `after`/`wait_ms` and always answers
    /// immediately with the plain shape.
    async fn old_hub() -> (String, tokio::task::JoinHandle<()>) {
        let current: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some("a".repeat(64))));
        let state = Arc::clone(&current);
        let router = axum::Router::new().route(
            "/api/head",
            axum::routing::get(move |_query: Query<HashMap<String, String>>| {
                let state = Arc::clone(&state);
                async move {
                    let current = state.lock().await.clone();
                    axum::Json(serde_json::json!({ "snapshot_id": current }))
                }
            }),
        );
        let (url, task) = serve(router).await;
        // Keep a way to advance the head from outside.
        let advance = current;
        let advance_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            *advance.lock().await = Some("b".repeat(64));
        });
        // Join the advance handle into the serve task lifecycle for the test.
        let task = tokio::spawn(async move {
            let _ = task.await;
            let _ = advance_task.await;
        });
        (url, task)
    }

    #[tokio::test]
    async fn old_hub_is_detected_and_never_busy_loops() {
        let (url, task) = old_hub().await;
        let api = ApiClient::new(&url, Some("token"));
        let outcome = wait_for_head_change(
            &api,
            "ws",
            Some(&"a".repeat(64)),
            Duration::from_millis(300),
        )
        .await
        .expect("wait request");
        assert!(
            matches!(outcome, HeadWaitOutcome::Unsupported(_)),
            "old hub must be detected: {outcome:?}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn observer_falls_back_to_bounded_polling_on_old_hubs() {
        let (url, task) = old_hub().await;
        let api = ApiClient::new(&url, Some("token"));
        let mut observer = HeadObserver::new(&api, "ws");
        observer.acknowledge(Some("a".repeat(64)));
        // The old hub advances its head after one second; the observer's
        // bounded polling must notice the change without spinning.
        let started = std::time::Instant::now();
        let observed = observer
            .observe(Duration::from_secs(5))
            .await
            .expect("observation");
        assert!(observed.changed, "polling fallback must observe the change");
        assert_eq!(observed.head.as_deref(), Some("b".repeat(64).as_str()));
        assert!(observed.unsupported);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(5),
            "fallback poll was not bounded: {elapsed:?}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn observer_retryable_failures_stay_within_the_window() {
        // A silent hub: the client read timeout bounds every request, and the
        // observer keeps the whole call within the requested window.
        let router = axum::Router::new().route(
            "/api/head",
            axum::routing::get(|| async {
                std::future::pending::<axum::response::Response>().await
            }),
        );
        let (url, task) = serve(router).await;
        let api = crate::api::ApiClient::new_with_timeouts(
            &url,
            Some("token"),
            Duration::from_secs(1),
            Duration::from_millis(300),
        )
        .expect("build bounded client");
        let mut observer = HeadObserver::new(&api, "ws");
        observer.acknowledge(Some("a".repeat(64)));
        let started = std::time::Instant::now();
        let result = observer.observe(Duration::from_secs(3)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(5),
            "observation stayed within its window: {elapsed:?}"
        );
        // Retryable transport failures must degrade to an unchanged
        // observation, never an error: watchers treat Err as fail-closed.
        let observed = result.expect("retryable failures stay observable");
        assert!(!observed.changed);
        task.abort();
    }

    /// A modern hub that reports wait support and never changes its head.
    async fn idle_modern_hub() -> (String, tokio::task::JoinHandle<()>) {
        let head = "a".repeat(64);
        let router = axum::Router::new().route(
            "/api/head",
            axum::routing::get(move |_query: Query<HashMap<String, String>>| {
                let head = head.clone();
                async move {
                    axum::Json(serde_json::json!({ "snapshot_id": head, "wait_supported": true }))
                }
            }),
        );
        serve(router).await
    }

    #[tokio::test]
    async fn unchanged_supported_hub_reports_unchanged_without_backstop() {
        let (url, task) = idle_modern_hub().await;
        let api = ApiClient::new(&url, Some("token"));
        let mut observer = HeadObserver::new(&api, "ws");
        observer.acknowledge(Some("a".repeat(64)));
        let observed = observer
            .observe(Duration::from_millis(300))
            .await
            .expect("observation");
        assert!(!observed.changed);
        assert!(!observed.unsupported);
        assert_eq!(observed.head.as_deref(), Some("a".repeat(64).as_str()));
        task.abort();
    }
}
