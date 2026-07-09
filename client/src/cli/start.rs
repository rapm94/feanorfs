use feanorfs_client::{
    do_sync, load_config, load_global_config, register_workspace, watch, ApiClient, ClientDb,
};
use feanorfs_common::looks_like_invite;
use std::path::{Path, PathBuf};

use super::util::{
    acquire_token, initialize_local_mirror, initialize_new_mirror, join_from_invite,
    link_existing_mirror, resolve_server_url,
};

#[derive(Debug, Clone)]
pub struct StartOptions {
    pub target: Option<String>,
    pub folder: Option<PathBuf>,
    pub workspace: String,
    pub encryption_key: Option<String>,
    pub server_token: Option<String>,
    pub lan: bool,
    pub local: bool,
    pub no_watch: bool,
}

enum ParsedTarget {
    Invite(String),
    ServerUrl(String),
    Folder(PathBuf),
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Bare `host:port` / `localhost:3030` — legacy setup URLs without a scheme.
fn looks_like_server_host(s: &str) -> bool {
    if s.contains('/') || looks_like_invite(s) {
        return false;
    }
    s.starts_with("localhost")
        || s.starts_with("127.0.0.1")
        || (s.contains(':') && !s.contains(' '))
}

fn normalize_server_url(s: &str) -> String {
    if looks_like_url(s) {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

fn looks_like_folder_path(s: &str) -> bool {
    s.starts_with('/')
        || s.starts_with('~')
        || s.starts_with('.')
        || (Path::new(s).is_dir() && !looks_like_server_host(s))
}

fn parse_target(raw: &str) -> anyhow::Result<ParsedTarget> {
    if looks_like_invite(raw) {
        return Ok(ParsedTarget::Invite(raw.to_string()));
    }
    if looks_like_url(raw) || looks_like_server_host(raw) {
        return Ok(ParsedTarget::ServerUrl(normalize_server_url(raw)));
    }
    if looks_like_folder_path(raw) {
        let expanded = if raw.starts_with('~') {
            let home = std::env::var("HOME").unwrap_or_else(|_| "~".into());
            raw.replacen('~', &home, 1)
        } else {
            raw.to_string()
        };
        return Ok(ParsedTarget::Folder(PathBuf::from(expanded)));
    }
    anyhow::bail!(
        "Unrecognized target `{raw}`. Use a server URL (http://…), invite (fnr1-…), or folder path."
    )
}

async fn finish_sync_watch(work_dir: &Path, no_watch: bool) -> anyhow::Result<()> {
    let config = load_config(work_dir)?;
    if config.format_version < 2 {
        eprintln!("Note: run `feanorfs migrate` to upgrade this workspace to format v2.");
    }
    let db = ClientDb::new(work_dir.join(".feanorfs")).await?;
    let api = ApiClient::from_config(work_dir, &config).await?;

    println!("Running sync...");
    do_sync(
        &api,
        &db,
        work_dir,
        &config.workspace_id,
        config.encryption_password.as_deref(),
        false,
    )
    .await?;

    let _ = register_workspace(work_dir);

    if !no_watch {
        watch::run_watch(
            &api,
            &db,
            work_dir,
            &config.workspace_id,
            config.encryption_password.as_deref(),
        )
        .await?;
    }
    Ok(())
}

pub async fn run_start(current_dir: &Path, opts: StartOptions) -> anyhow::Result<()> {
    let mut folder = opts.folder.clone();
    let mut invite: Option<String> = None;
    let mut server_url: Option<String> = None;

    if let Some(ref raw) = opts.target {
        match parse_target(raw)? {
            ParsedTarget::Invite(s) => invite = Some(s),
            ParsedTarget::ServerUrl(u) => server_url = Some(u),
            ParsedTarget::Folder(p) => {
                if folder.is_some() {
                    anyhow::bail!("Specify either one folder positional or `--folder`, not both.");
                }
                folder = Some(p);
            }
        }
    }

    let work_dir = folder.clone().unwrap_or_else(|| current_dir.to_path_buf());
    if work_dir != current_dir {
        std::env::set_current_dir(&work_dir)?;
    }
    let work_dir = std::env::current_dir()?;

    let has_config = load_config(&work_dir).is_ok();
    let is_relink = opts.encryption_key.is_some() || invite.is_some();
    let wants_setup = server_url.is_some()
        || opts.local
        || opts.encryption_key.is_some()
        || opts.lan
        || invite.is_some()
        || (!has_config && load_global_config().is_ok());

    if has_config && wants_setup && !is_relink {
        anyhow::bail!(
            "Workspace already configured in this folder. Use `feanorfs sync` to resume, \
             or pass an invite / `--encryption-key` to re-link."
        );
    }

    if let Some(token) = invite {
        join_from_invite(&work_dir, &token, false).await?;
        return finish_sync_watch(&work_dir, opts.no_watch).await;
    }

    if opts.local {
        initialize_local_mirror(
            &work_dir,
            opts.workspace.clone(),
            opts.encryption_key.clone(),
        )
        .await?;
        return finish_sync_watch(&work_dir, opts.no_watch).await;
    }

    if let Some(url) = server_url {
        let token = acquire_token(&url, opts.server_token.clone()).await?;
        initialize_new_mirror(
            &work_dir,
            url,
            opts.workspace.clone(),
            opts.encryption_key.clone(),
            token,
            true,
            false,
        )
        .await?;
        return finish_sync_watch(&work_dir, opts.no_watch).await;
    }

    if let Some(ref key) = opts.encryption_key {
        let url = resolve_server_url(None, opts.lan)?;
        link_existing_mirror(
            &work_dir,
            url,
            opts.workspace.clone(),
            key.clone(),
            opts.server_token.clone(),
            false,
            false,
        )
        .await?;
        return finish_sync_watch(&work_dir, opts.no_watch).await;
    }

    if has_config {
        return finish_sync_watch(&work_dir, opts.no_watch).await;
    }

    if opts.lan {
        let url = resolve_server_url(None, true)?;
        let token = acquire_token(&url, opts.server_token.clone()).await?;
        initialize_new_mirror(
            &work_dir,
            url,
            opts.workspace.clone(),
            None,
            token,
            true,
            false,
        )
        .await?;
        return finish_sync_watch(&work_dir, opts.no_watch).await;
    }

    if let Ok(global) = load_global_config() {
        let url = global.server_url;
        let token = acquire_token(&url, opts.server_token.or(global.server_password)).await?;
        initialize_new_mirror(
            &work_dir,
            url,
            opts.workspace.clone(),
            None,
            token,
            false,
            false,
        )
        .await?;
        return finish_sync_watch(&work_dir, opts.no_watch).await;
    }

    anyhow::bail!(
        "No workspace configured here yet.\n\
         \n\
           feanorfs start https://your-server:3030     create on a server\n\
           feanorfs start fnr1-…                       join from invite\n\
           feanorfs start ~/projects/my-app           resume/create in folder\n\
           feanorfs start --local                      embedded local hub\n\
           feanorfs start --lan                        discover server on LAN"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_invite_target() {
        let t = "fnr1-deadbeef";
        assert!(matches!(parse_target(t).unwrap(), ParsedTarget::Invite(_)));
    }

    #[test]
    fn parse_url_target() {
        assert!(matches!(
            parse_target("https://x:3030").unwrap(),
            ParsedTarget::ServerUrl(_)
        ));
    }

    #[test]
    fn parse_legacy_host_port() {
        match parse_target("127.0.0.1:3030").unwrap() {
            ParsedTarget::ServerUrl(u) => assert_eq!(u, "http://127.0.0.1:3030"),
            _ => panic!("expected server url"),
        }
    }

    #[test]
    fn parse_folder_target() {
        assert!(matches!(
            parse_target("/tmp/ws").unwrap(),
            ParsedTarget::Folder(_)
        ));
    }
}
