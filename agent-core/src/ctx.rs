use crate::api::ApiClient;
use crate::local::{ClientDb, Config};
use anyhow::Result;
use feanorfs_common::LegacyPolicy;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static WARNED_EMPTY_PASSWORD: OnceLock<()> = OnceLock::new();

/// Sync context passed through upload/download/conflict paths.
pub struct SyncCtx<'a> {
    pub api: &'a ApiClient,
    pub db: &'a ClientDb,
    pub base: &'a Path,
    pub policy: LegacyPolicy,
    workspace_id: std::borrow::Cow<'a, str>,
    password: std::borrow::Cow<'a, str>,
    format_version: u32,
    state_dir_cache: Mutex<Option<std::path::PathBuf>>,
}

impl<'a> SyncCtx<'a> {
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn password_str(&self) -> &str {
        if self.password.is_empty() {
            WARNED_EMPTY_PASSWORD.get_or_init(|| {
                tracing::warn!(
                    "No E2EE password set in config. Using insecure legacy default. \
                     Run 'feanorfs setup' to set a proper encryption key."
                );
            });
            feanorfs_common::LEGACY_DEFAULT_PASSWORD
        } else {
            &self.password
        }
    }

    pub fn password(&self) -> Option<&str> {
        if self.password.is_empty() {
            None
        } else {
            Some(&self.password)
        }
    }

    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    pub fn state_dir(&self) -> Result<std::path::PathBuf> {
        let mut cache = self
            .state_dir_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state_dir) = cache.as_ref() {
            return Ok(state_dir.clone());
        }
        let state_dir = crate::workspace_layout::ensure_workspace_state(self.base)?;
        *cache = Some(state_dir.clone());
        Ok(state_dir)
    }

    /// Build a context from an explicit policy.
    #[must_use]
    pub fn new(
        api: &'a ApiClient,
        db: &'a ClientDb,
        base: &'a Path,
        workspace_id: &str,
        password: Option<&str>,
        policy: LegacyPolicy,
    ) -> Self {
        Self {
            api,
            db,
            base,
            policy,
            workspace_id: std::borrow::Cow::Owned(workspace_id.to_string()),
            password: std::borrow::Cow::Owned(password.unwrap_or("").to_string()),
            format_version: 2,
            state_dir_cache: Mutex::new(None),
        }
    }

    /// Build a context with an explicit verified format version. Use when the
    /// caller already loaded the workspace config and must not downgrade
    /// format-gated operations (agent signals require format v3).
    #[must_use]
    pub fn with_format_version(
        api: &'a ApiClient,
        db: &'a ClientDb,
        base: &'a Path,
        workspace_id: &str,
        password: Option<&str>,
        policy: LegacyPolicy,
        format_version: u32,
    ) -> Self {
        let mut ctx = Self::new(api, db, base, workspace_id, password, policy);
        ctx.format_version = format_version;
        ctx
    }
    /// Build a context from a loaded `Config`.
    pub fn from_config(
        api: &'a ApiClient,
        db: &'a ClientDb,
        base: &'a Path,
        config: &Config,
    ) -> Result<Self> {
        Ok(Self {
            api,
            db,
            base,
            policy: crate::paths::legacy_policy_for_config(config),
            workspace_id: std::borrow::Cow::Owned(config.workspace_id.clone()),
            password: std::borrow::Cow::Owned(
                config.encryption_password.clone().unwrap_or_default(),
            ),
            format_version: config.format_version,
            state_dir_cache: Mutex::new(None),
        })
    }

    /// Build a context whose cache/object state is owned outside `base`'s
    /// normal top-level workspace registration.
    pub(crate) fn from_config_with_state_dir(
        api: &'a ApiClient,
        db: &'a ClientDb,
        base: &'a Path,
        config: &Config,
        state_dir: std::path::PathBuf,
    ) -> Result<Self> {
        let mut ctx = Self::from_config(api, db, base, config)?;
        ctx.state_dir_cache = Mutex::new(Some(state_dir));
        Ok(ctx)
    }
}
