use crate::api::ApiClient;
use crate::commands::{do_pull_only, do_push_only};
use crate::local::{load_config, save_config, validate_e2ee_key, ClientDb, Config};
use anyhow::Result;
use feanorfs_common::LegacyPolicy;
use std::path::Path;

/// Re-seal all local blobs as AEAD and bump workspace to format v2.
pub async fn migrate_workspace(base: &Path, rekey: bool) -> Result<()> {
    let mut config = load_config(base)?;
    let db = ClientDb::new(base.join(".feanorfs")).await?;
    let api = ApiClient::from_config(base, &config).await?;
    let mut password = config
        .encryption_password
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no encryption key configured"))?;
    let old_password = password.clone();
    let new_key = if rekey {
        let k = feanorfs_common::generate_password()?;
        validate_e2ee_key(&k, 2)?;
        println!("Generated new encryption key. Pulling with current key, then re-uploading sealed with the new key...");
        Some(k)
    } else {
        None
    };

    println!("Pulling latest from mirror...");
    do_pull_only(
        &api,
        &db,
        base,
        &config.workspace_id,
        Some(old_password.as_str()),
        false,
    )
    .await?;

    let cached_after_pull = db.get_cache_entries().await?;
    let dehydrated: Vec<String> = cached_after_pull
        .iter()
        .filter(|(_, e)| !e.hydrated && e.deleted_at.is_none())
        .map(|(p, _)| p.clone())
        .collect();
    if !dehydrated.is_empty() {
        anyhow::bail!(
            "Cannot migrate with unhydrated placeholders. Run `feanorfs hydrate` first: {}",
            dehydrated.join(", ")
        );
    }

    if let Some(ref nk) = new_key {
        config.encryption_password = Some(nk.clone());
        save_config(base, &config)?;
        password = nk.clone();
    }

    // Invalidate every cache row so the upcoming push re-hashes, re-seals with
    // AEAD, and re-uploads. Without this, `do_push_only` sees an unchanged cache
    // and uploads nothing — leaving legacy XOR blobs on the server while the
    // config flips to v2, which then hard-fails every decrypt with `Reject`.
    let entries = db.get_cache_entries().await?;
    let mut resealed = 0u32;
    for (path, entry) in &entries {
        if entry.deleted_at.is_some() {
            continue;
        }
        db.delete_cache_entry(path).await?;
        resealed += 1;
    }

    println!("Pushing re-sealed blobs...");
    do_push_only(
        &api,
        &db,
        base,
        &config.workspace_id,
        Some(password.as_str()),
    )
    .await?;

    config.format_version = 2;
    save_config(base, &config)?;

    if let Some(ref nk) = new_key {
        println!("New encryption key (save this — share with other machines):");
        println!("{nk}");
    }

    println!(
        "Migration complete. Workspace is now format v2 (AEAD-only). \
         Re-sealed {resealed} file(s). Run migrate on other machines."
    );
    Ok(())
}

pub fn legacy_policy_for_config(config: &Config) -> LegacyPolicy {
    feanorfs_agent_core::legacy_policy_for_config(config)
}
