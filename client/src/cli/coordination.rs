//! `agent next` (unified coordination lifecycle) and `agent guard` (edit
//! guard, including the harness hook adapter).

use feanorfs_client::{agent_identity, coordination_status, guard_paths, load_config};
use feanorfs_common::CapabilityRoster;
use feanorfs_common::{
    ClaimOutcome, ClaimResult, CoordinationStatus, DoneResult, GuardFinding, GuardResult,
    GuardVerdict, LifecycleStage, WorkVerificationStatus, COORDINATION_SCHEMA_VERSION,
};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use super::agent::control_workspace_root;
use super::util::{output_json, terminal_line};

/// Exit status a harness hook interprets as "block this tool call".
const HOOK_BLOCK_EXIT: i32 = 2;
/// Hook payloads are small JSON objects; refuse anything larger.
const HOOK_INPUT_MAX_BYTES: u64 = 1024 * 1024;

pub async fn run_next(
    current_dir: &Path,
    agent: Option<&str>,
    wait: Option<std::time::Duration>,
    json: bool,
) -> anyhow::Result<()> {
    let root = control_workspace_root(current_dir)?;
    let config = load_config(&root)?;
    let db = crate::open_client_db(&root).await?;
    let api = crate::open_api_client(&root, &config).await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, &root, &config)?;
    let agent = agent_identity(agent);
    let status = match wait {
        Some(wait) => feanorfs_client::coordination_status_wait(&ctx, &agent, wait).await?,
        None => coordination_status(&ctx, &agent).await?,
    };
    if json {
        output_json(&status)
    } else {
        print!("{}", render_next(&status));
        Ok(())
    }
}

async fn with_ctx<T>(
    current_dir: &Path,
    op: impl AsyncFnOnce(&feanorfs_client::SyncCtx<'_>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let root = control_workspace_root(current_dir)?;
    let config = load_config(&root)?;
    let db = crate::open_client_db(&root).await?;
    let api = crate::open_api_client(&root, &config).await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, &root, &config)?;
    op(&ctx).await
}

pub async fn run_capabilities(
    current_dir: &Path,
    agent: Option<&str>,
    announce: Option<Vec<String>>,
    json: bool,
) -> anyhow::Result<()> {
    let agent = agent_identity(agent);
    let roster = with_ctx(current_dir, async |ctx| {
        feanorfs_client::capabilities(ctx, &agent, announce).await
    })
    .await?;
    if json {
        return output_json(&roster);
    }
    print!("{}", render_roster(&roster));
    Ok(())
}

/// Announces capabilities for `agent run` before the child starts.
pub async fn announce_quietly(
    current_dir: &Path,
    agent: &str,
    capabilities: Vec<String>,
) -> anyhow::Result<()> {
    with_ctx(current_dir, async |ctx| {
        feanorfs_client::capabilities(ctx, agent, Some(capabilities)).await
    })
    .await
    .map(|_| ())
}

/// Finishes the agent's claimed work after `agent run`'s command exits
/// cleanly with every edit settled, so harnesses without a Stop hook need no
/// `agent done` turn. A failure only leaves the claim open.
pub async fn finish_quietly(current_dir: &Path, agent: &str) {
    let finished = with_ctx(current_dir, async |ctx| {
        feanorfs_client::finish_work(
            ctx,
            agent,
            Some("finished when `agent run` exited; no verification reported"),
            None,
            Duration::from_secs(30),
        )
        .await
    })
    .await;
    match finished {
        Ok(done) if !done.completed.is_empty() => {
            eprintln!(
                "Completed {} claimed task(s) for '{agent}'.",
                done.completed.len()
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!(
            "FeanorFS: claimed work left open ({error:#}); run `feanorfs agent done --for {agent}`."
        ),
    }
}

fn render_roster(roster: &CapabilityRoster) -> String {
    let mut out = String::new();
    if let Some(id) = &roster.announced {
        out.push_str(&format!("Announced (signal {}).\n", &id[..id.len().min(8)]));
    }
    if roster.agents.is_empty() {
        out.push_str("No agent advertises capabilities yet.\n");
    }
    for entry in &roster.agents {
        out.push_str(&format!(
            "  {:<20} {}\n",
            terminal_line(&entry.agent),
            terminal_line(&entry.capabilities.join(", "))
        ));
    }
    if roster.projection_incomplete {
        out.push_str("Roster incomplete: older announcements may be missing.\n");
    }
    out
}

fn render_next(status: &CoordinationStatus) -> String {
    let mut out = String::new();
    let open: Vec<_> = status
        .items
        .iter()
        .filter(|item| !item.stage.is_terminal())
        .collect();
    if open.is_empty() {
        out.push_str(&format!("Nothing in flight for '{}'.\n", status.agent));
    } else {
        out.push_str(&format!("In flight for '{}':\n", status.agent));
        for item in open {
            let owner = item
                .owner
                .as_deref()
                .map(|owner| format!(" [{owner}]"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {:<14} {}{owner}: {}\n",
                stage_label(item.stage),
                terminal_line(&item.id),
                terminal_line(&item.detail)
            ));
        }
    }
    if !status.next_actions.is_empty() {
        out.push_str("Next:\n");
        for action in &status.next_actions {
            let who = if action.actor == status.agent {
                "you".to_string()
            } else {
                action.actor.clone()
            };
            out.push_str(&format!(
                "  {who}: {}\n      {}\n",
                terminal_line(&action.cli),
                terminal_line(&action.reason)
            ));
        }
    }
    for warning in &status.warnings {
        out.push_str(&format!("Warning: {}\n", terminal_line(warning)));
    }
    if status.projection_incomplete {
        out.push_str(
            "Projection incomplete: absent items are not proof that nothing is pending.\n",
        );
    }
    out
}

fn stage_label(stage: LifecycleStage) -> &'static str {
    match stage {
        LifecycleStage::AwaitingHuman => "awaiting-human",
        LifecycleStage::Conflicted => "conflicted",
        LifecycleStage::Proposed => "proposed",
        LifecycleStage::Assigned => "assigned",
        LifecycleStage::Resolving => "resolving",
        LifecycleStage::Accepted => "accepted",
        LifecycleStage::Settled => "settled",
        LifecycleStage::Blocked => "blocked",
        LifecycleStage::Stopped => "stopped",
        LifecycleStage::Done => "done",
    }
}

pub struct GuardArgs {
    pub paths: Vec<String>,
    pub agent: Option<String>,
    pub require_scope: bool,
    pub hook: bool,
    /// Claim unclaimed paths first, waiting up to this long for a decision.
    pub claim: Option<Duration>,
}

/// Runs the guard. Exits with status 2 on `deny` so harness hooks block the
/// tool call. In `--hook` mode every internal failure allows the edit: the
/// guard is advisory coordination, never access control.
pub async fn run_guard(current_dir: &Path, args: GuardArgs, json: bool) -> anyhow::Result<()> {
    let hook = args.hook;
    let outcome = guard(current_dir, args).await;
    let result = match outcome {
        Ok(Some(result)) => result,
        Ok(None) => return Ok(()),
        Err(error) if hook => {
            eprintln!("feanorfs guard skipped: {error:#}");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if json && !hook {
        output_json(&result)?;
    }
    for finding in &result.findings {
        let label = match finding.verdict {
            GuardVerdict::Deny => "blocked",
            GuardVerdict::Warn => "warning",
            GuardVerdict::Allow => continue,
        };
        eprintln!(
            "FeanorFS {label}: {}: {}",
            terminal_line(&finding.path),
            terminal_line(&finding.reason)
        );
    }
    if result.verdict == GuardVerdict::Deny {
        std::process::exit(HOOK_BLOCK_EXIT);
    }
    if !json && !hook && result.findings.is_empty() {
        println!("Allowed.");
    }
    Ok(())
}

async fn guard(current_dir: &Path, args: GuardArgs) -> anyhow::Result<Option<GuardResult>> {
    let root = control_workspace_root(current_dir)?;
    // Probe without creating state: a globally installed hook runs in every
    // project, and unrelated folders must not gain workspace-state slots.
    if !feanorfs_agent_core::workspace_has_preferred_state(&root) {
        anyhow::ensure!(
            args.hook,
            "'{}' is not a FeanorFS workspace",
            root.display()
        );
        return Ok(None);
    }
    let roots = guard_roots(&root);
    let mut paths = Vec::new();
    let raw = if args.hook { hook_paths()? } else { args.paths };
    for raw in raw {
        if let Some(path) = workspace_relative(&roots, current_dir, &raw) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    if paths.is_empty() {
        return Ok(None);
    }
    let config = load_config(&root)?;
    let db = crate::open_client_db(&root).await?;
    let api = crate::open_api_client(&root, &config).await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, &root, &config)?;
    let agent = agent_identity(args.agent.as_deref());
    if let Some(wait) = args.claim {
        let claim = feanorfs_client::claim_scope(&ctx, &agent, &paths, None, wait).await?;
        if !matches!(
            claim.outcome,
            ClaimOutcome::Covered | ClaimOutcome::Accepted
        ) {
            let reason = format!(
                "scope claim {}: {}",
                claim_label(claim.outcome),
                claim.reason.as_deref().unwrap_or("no decision")
            );
            return Ok(Some(GuardResult {
                schema_version: COORDINATION_SCHEMA_VERSION,
                agent,
                verdict: GuardVerdict::Deny,
                findings: paths
                    .iter()
                    .map(|path| GuardFinding {
                        path: path.clone(),
                        verdict: GuardVerdict::Deny,
                        reason: reason.clone(),
                    })
                    .collect(),
            }));
        }
    }
    Ok(Some(
        guard_paths(&ctx, &agent, &paths, args.require_scope).await?,
    ))
}

fn claim_label(outcome: ClaimOutcome) -> &'static str {
    match outcome {
        ClaimOutcome::Covered => "already held",
        ClaimOutcome::Accepted => "accepted",
        ClaimOutcome::Pending => "still pending",
        ClaimOutcome::Rejected => "rejected",
    }
}

/// `agent claim`: one call to propose scope and wait for the decision.
/// Exits 0 when held or accepted, 3 when still pending, 1 when rejected.
pub async fn run_claim(
    current_dir: &Path,
    agent: Option<&str>,
    paths: Vec<String>,
    coordinator: Option<&str>,
    wait: Duration,
    json: bool,
) -> anyhow::Result<()> {
    let agent = agent_identity(agent);
    let roots = guard_roots(&control_workspace_root(current_dir)?);
    let paths: Vec<String> = paths
        .iter()
        .map(|raw| {
            if raw.ends_with("/**") {
                Some(raw.clone())
            } else {
                workspace_relative(&roots, current_dir, raw)
            }
        })
        .collect::<Option<_>>()
        .ok_or_else(|| anyhow::anyhow!("every path must be inside the workspace"))?;
    let claim: ClaimResult = with_ctx(current_dir, async |ctx| {
        feanorfs_client::claim_scope(ctx, &agent, &paths, coordinator, wait).await
    })
    .await?;
    if json {
        output_json(&claim)?;
    } else {
        let detail = claim
            .reason
            .as_deref()
            .map(|reason| format!(" ({})", terminal_line(reason)))
            .unwrap_or_default();
        println!(
            "Claim {}: {}{detail}",
            claim_label(claim.outcome),
            terminal_line(&claim.paths.join(", "))
        );
    }
    match claim.outcome {
        ClaimOutcome::Covered | ClaimOutcome::Accepted => Ok(()),
        ClaimOutcome::Pending => std::process::exit(3),
        ClaimOutcome::Rejected => std::process::exit(1),
    }
}

pub struct DoneArgs {
    pub agent: Option<String>,
    pub summary: Option<String>,
    pub verification: Option<WorkVerificationStatus>,
    pub wait: Duration,
    pub hook: bool,
}

/// `agent done`: one call to wait for edits to land, then settle and
/// complete. As a Stop hook (`--hook`), a failure blocks stopping once so
/// the agent can fix it, and never loops (`stop_hook_active`).
pub async fn run_done(current_dir: &Path, args: DoneArgs, json: bool) -> anyhow::Result<()> {
    let retrying =
        args.hook && hook_payload()?.get("stop_hook_active") == Some(&serde_json::json!(true));
    let root = control_workspace_root(current_dir)?;
    if args.hook && !feanorfs_agent_core::workspace_has_preferred_state(&root) {
        return Ok(());
    }
    let agent = agent_identity(args.agent.as_deref());
    let outcome: anyhow::Result<DoneResult> = with_ctx(current_dir, async |ctx| {
        feanorfs_client::finish_work(
            ctx,
            &agent,
            args.summary.as_deref(),
            args.verification,
            args.wait,
        )
        .await
    })
    .await;
    match outcome {
        Ok(done) if json => output_json(&done),
        Ok(done) => {
            if !args.hook || !done.completed.is_empty() {
                println!(
                    "Completed {} task(s) for '{}'.",
                    done.completed.len(),
                    done.agent
                );
            }
            Ok(())
        }
        Err(error) if args.hook && !retrying => {
            eprintln!("FeanorFS: work not finished: {error:#}");
            std::process::exit(HOOK_BLOCK_EXIT);
        }
        Err(error) if args.hook => {
            eprintln!("FeanorFS: work not finished: {error:#}");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// `agent coordinate`: accept proposals addressed to `coordinator` whose
/// scope overlaps no other agent's live scope; with `watch`, keep going.
pub async fn run_coordinate(
    current_dir: &Path,
    coordinator: Option<&str>,
    watch: bool,
    json: bool,
) -> anyhow::Result<()> {
    let coordinator = coordinator.unwrap_or("human").to_string();
    loop {
        let pass = with_ctx(current_dir, async |ctx| {
            feanorfs_client::coordinate_pass(ctx, &coordinator).await
        })
        .await?;
        if json {
            output_json(&pass)?;
        } else if !pass.accepted.is_empty() || !watch {
            println!(
                "Accepted {} proposal(s); {} waiting on overlapping scope.",
                pass.accepted.len(),
                pass.waiting.len()
            );
        }
        if !watch {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The agent worktree (inside `agent run`) first, then the shared root.
fn guard_roots(control_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = std::env::var_os("FEANORFS_AGENT_DIR") {
        roots.push(PathBuf::from(dir));
    }
    roots.push(control_root.to_path_buf());
    roots
        .into_iter()
        .filter_map(|root| root.canonicalize().ok())
        .collect()
}

/// Paths named by a harness PreToolUse payload (`tool_input.file_path`,
/// `path`, or `notebook_path`), resolved against the payload `cwd`.
fn hook_paths() -> anyhow::Result<Vec<String>> {
    Ok(paths_from_hook_payload(&read_hook_input()?))
}

fn read_hook_input() -> anyhow::Result<String> {
    let mut input = String::new();
    std::io::stdin()
        .take(HOOK_INPUT_MAX_BYTES + 1)
        .read_to_string(&mut input)?;
    anyhow::ensure!(
        input.len() as u64 <= HOOK_INPUT_MAX_BYTES,
        "hook payload exceeds 1 MiB"
    );
    Ok(input)
}

/// The harness hook payload as JSON (`null` when unparseable).
fn hook_payload() -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::from_str(&read_hook_input()?).unwrap_or(serde_json::Value::Null))
}

fn paths_from_hook_payload(input: &str) -> Vec<String> {
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(input) else {
        return Vec::new();
    };
    let cwd = payload.get("cwd").and_then(|value| value.as_str());
    let Some(tool_input) = payload.get("tool_input") else {
        return Vec::new();
    };
    ["file_path", "path", "notebook_path"]
        .iter()
        .filter_map(|key| tool_input.get(*key).and_then(|value| value.as_str()))
        .map(|path| match cwd {
            Some(cwd) if Path::new(path).is_relative() => {
                Path::new(cwd).join(path).to_string_lossy().into_owned()
            }
            _ => path.to_string(),
        })
        .collect()
}

/// Converts a raw path to a canonical workspace-relative path under one of
/// `roots`, or `None` when it lies outside every workspace root.
fn workspace_relative(roots: &[PathBuf], current_dir: &Path, raw: &str) -> Option<String> {
    let path = Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    };
    let absolute = canonicalize_existing_prefix(&absolute)?;
    roots.iter().find_map(|root| {
        let relative = absolute.strip_prefix(root).ok()?;
        let parts: Vec<String> = relative
            .components()
            .map(|component| match component {
                Component::Normal(part) => part.to_str().map(str::to_string),
                _ => None,
            })
            .collect::<Option<_>>()?;
        (!parts.is_empty()).then(|| parts.join("/"))
    })
}

/// Canonicalizes the longest existing ancestor and re-appends the rest, so
/// files about to be created still resolve through symlinked roots.
fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        if let Ok(canonical) = current.canonicalize() {
            let mut result = canonical;
            for part in missing.iter().rev() {
                result.push(part);
            }
            return Some(result);
        }
        missing.push(current.file_name()?.to_os_string());
        current = current.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_payload_paths_resolve_against_cwd() {
        let payload = r#"{"cwd":"/w","tool_name":"Edit","tool_input":{"file_path":"src/a.rs","old_string":"x"}}"#;
        assert_eq!(
            paths_from_hook_payload(payload),
            vec!["/w/src/a.rs".to_string()]
        );
        let notebook = r#"{"tool_input":{"notebook_path":"/w/n.ipynb"}}"#;
        assert_eq!(
            paths_from_hook_payload(notebook),
            vec!["/w/n.ipynb".to_string()]
        );
        assert!(paths_from_hook_payload("not json").is_empty());
        assert!(paths_from_hook_payload(r#"{"tool_input":{"command":"ls"}}"#).is_empty());
    }

    #[test]
    fn relative_paths_are_canonical_and_outside_paths_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        let roots = vec![root.clone()];
        assert_eq!(
            workspace_relative(&roots, &root, "src/new.rs").as_deref(),
            Some("src/new.rs")
        );
        assert_eq!(
            workspace_relative(&roots, &root, root.join("src/x/y.rs").to_str().unwrap()).as_deref(),
            Some("src/x/y.rs")
        );
        assert_eq!(
            workspace_relative(&roots, &root, "/elsewhere/file.rs"),
            None
        );
        assert_eq!(workspace_relative(&roots, &root, "."), None);
    }
}
