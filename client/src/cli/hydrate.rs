#![allow(unused_imports)]
use clap::Subcommand;
use feanorfs_client::ApiClient;
use feanorfs_client::ClientDb;
use feanorfs_client::{commands, load_config, predictive};
use std::io::Write as _;
use std::path::Path;

use super::util::output_json;

#[derive(Subcommand)]
pub enum HydrateAction {
    /// Download and decrypt deferred lazy placeholder files
    Hydrate {
        /// A specific file to hydrate. If omitted, hydrates all placeholder files.
        path: Option<String>,
    },
    /// Print a file's contents, downloading and decrypting it first if it is not hydrated
    Cat {
        /// The relative path of the file to display
        path: String,
    },
}

pub async fn run(current_dir: &Path, action: HydrateAction, json: bool) -> anyhow::Result<()> {
    match action {
        HydrateAction::Hydrate { path } => run_hydrate(current_dir, json, path).await,
        HydrateAction::Cat { path } => run_cat(current_dir, json, path).await,
    }
}

async fn open(
    current_dir: &Path,
) -> anyhow::Result<(feanorfs_client::Config, ClientDb, ApiClient)> {
    let config = load_config(current_dir)?;
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    Ok((config, db, api))
}

async fn run_hydrate(current_dir: &Path, json: bool, path: Option<String>) -> anyhow::Result<()> {
    let (config, db, api) = open(current_dir).await?;
    let result = commands::do_hydrate(
        &api,
        &db,
        current_dir,
        path.clone(),
        config.encryption_password.as_deref(),
    )
    .await?;

    if let Some(ref p) = path {
        if let Err(e) = predictive::record_access_with_recent(&db, p).await {
            tracing::warn!("Failed to record predictive access for {p}: {e:#}");
        }
        if let Err(e) = predictive::prefetch_related(
            current_dir,
            &db,
            &api,
            config.encryption_password.as_deref(),
            std::slice::from_ref(p),
        )
        .await
        {
            tracing::warn!("Predictive prefetch failed for {p}: {e:#}");
        }
    }

    if json {
        output_json(&result)?;
    } else {
        println!("{}", result.message);
    }
    Ok(())
}

async fn run_cat(current_dir: &Path, json: bool, path: String) -> anyhow::Result<()> {
    let (config, db, api) = open(current_dir).await?;
    let result = commands::do_cat(
        &api,
        &db,
        current_dir,
        &path,
        config.encryption_password.as_deref(),
    )
    .await?;

    if let Err(e) = predictive::record_access_with_recent(&db, &path).await {
        tracing::warn!("Failed to record predictive access for {path}: {e:#}");
    }

    if json {
        output_json(&result)?;
    } else {
        if result.untracked {
            println!("Warning: file '{path}' is not tracked. Reading directly.");
        }
        if result.hydrated_first {
            eprintln!("Hydrated {path} from server.");
        }
        if result.not_found {
            println!("Error: file '{path}' does not exist.");
        } else {
            std::io::stdout().write_all(&result.content)?;
        }
    }
    Ok(())
}
