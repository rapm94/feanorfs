use crate::api::ApiClient;
use crate::local::{ClientDb, Config};
use anyhow::Result;
use feanorfs_common::LegacyPolicy;
use std::path::Path;
use std::sync::OnceLock;

static WARNED_EMPTY_PASSWORD: OnceLock<()> = OnceLock::new();

/// Sync context passed through upload/download/conflict paths.
pub struct SyncCtx<'a> {
    pub api: &'a ApiClient,
    pub db: &'a ClientDb,
    pub base: &'a Path,
    pub policy: LegacyPolicy,
    workspace_id: std::borrow::Cow<'a, str>,
    password: std::borrow::Cow<'a, str>,
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
        }
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
        })
    }
}
