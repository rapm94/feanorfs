use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const SKILL_MD: &str = include_str!("../../../skills/feanorfs-collaboration/SKILL.md");
const PROTOCOL_MD: &str =
    include_str!("../../../skills/feanorfs-collaboration/references/protocol.md");
const OPENAI_YAML: &str = include_str!("../../../skills/feanorfs-collaboration/agents/openai.yaml");

const MANAGED_MARKER: &str = ".feanorfs-managed";

#[derive(Parser)]
pub struct IntegrateCli {
    #[command(subcommand)]
    action: Option<IntegrateSubcommand>,

    /// Host to target (claude, cursor, gemini, opencode, codex; all detected when omitted)
    #[arg(long, global = true)]
    host: Option<String>,

    /// Configure for explicit project/workspace scope instead of global user scope.
    #[arg(long, global = true, value_name = "PATH")]
    project: Option<PathBuf>,

    /// Overwrite conflicting user-owned entries instead of skipping them.
    #[arg(long, global = true)]
    force: bool,
}

#[derive(Subcommand, Clone, Copy, PartialEq, Eq)]
enum IntegrateSubcommand {
    /// Detect supported hosts and install FeanorFS MCP + skill (default action)
    Install,
    /// List supported hosts, detection state, and configuration status
    Status,
    /// Remove FeanorFS MCP registration and installed skills
    Uninstall,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum HostKind {
    Claude,
    Cursor,
    Gemini,
    OpenCode,
    Codex,
}

impl HostKind {
    const ALL: &'static [HostKind] = &[
        HostKind::Claude,
        HostKind::Cursor,
        HostKind::Gemini,
        HostKind::OpenCode,
        HostKind::Codex,
    ];

    fn id(self) -> &'static str {
        match self {
            HostKind::Claude => "claude",
            HostKind::Cursor => "cursor",
            HostKind::Gemini => "gemini",
            HostKind::OpenCode => "opencode",
            HostKind::Codex => "codex",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            HostKind::Claude => "Claude Desktop / Code",
            HostKind::Cursor => "Cursor",
            HostKind::Gemini => "Gemini / Antigravity",
            HostKind::OpenCode => "OpenCode",
            HostKind::Codex => "Codex",
        }
    }

    fn parse(s: &str) -> Option<HostKind> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-desktop" | "claude-code" => Some(HostKind::Claude),
            "cursor" => Some(HostKind::Cursor),
            "gemini" | "antigravity" => Some(HostKind::Gemini),
            "opencode" => Some(HostKind::OpenCode),
            "codex" => Some(HostKind::Codex),
            _ => None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum IntegrationStatus {
    NotDetected,
    Configured,
    PartiallyConfigured,
    NotConfigured,
    Conflict,
}

#[derive(Serialize)]
struct HostReport {
    host: HostKind,
    name: String,
    detected: bool,
    mcp_config_path: PathBuf,
    mcp_registered: bool,
    skill_path: PathBuf,
    skill_installed: bool,
    status: IntegrationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_message: Option<String>,
}

#[derive(Serialize)]
struct IntegrationReport {
    scope: String,
    hosts: Vec<HostReport>,
    installed: Vec<String>,
    removed: Vec<String>,
    conflicts: Vec<String>,
    warnings: Vec<String>,
}

struct EnvironmentContext {
    home_dir: PathBuf,
    config_dir: PathBuf,
    #[cfg(target_os = "windows")]
    app_data_dir: PathBuf,
}

impl EnvironmentContext {
    fn real() -> Result<Self> {
        let home_dir = if let Some(custom) = std::env::var_os("FEANORFS_HOME") {
            PathBuf::from(custom)
        } else if let Some(home) =
            std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        {
            PathBuf::from(home)
        } else {
            bail!("could not resolve user home directory (HOME or USERPROFILE not set)");
        };

        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".config"));

        #[cfg(target_os = "windows")]
        let app_data_dir = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join("AppData").join("Roaming"));

        Ok(Self {
            home_dir,
            config_dir,
            #[cfg(target_os = "windows")]
            app_data_dir,
        })
    }

    #[cfg(test)]
    fn mock(root: &Path) -> Self {
        Self {
            home_dir: root.to_path_buf(),
            config_dir: root.join(".config"),
            #[cfg(target_os = "windows")]
            app_data_dir: root.join("AppData"),
        }
    }
}

struct HostPaths {
    mcp_config_path: PathBuf,
    skill_dir: PathBuf,
    detection_markers: Vec<PathBuf>,
    uses_opencode_format: bool,
}

impl HostPaths {
    fn for_host(host: HostKind, env: &EnvironmentContext, project: Option<&Path>) -> Self {
        if let Some(project) = project {
            Self::for_project_scope(host, project)
        } else {
            Self::for_global_scope(host, env)
        }
    }

    fn for_project_scope(host: HostKind, project: &Path) -> Self {
        match host {
            HostKind::Claude => Self {
                mcp_config_path: project.join(".claude").join("mcp.json"),
                skill_dir: project.join(".claude").join("skills"),
                detection_markers: vec![project.join(".claude")],
                uses_opencode_format: false,
            },
            HostKind::Cursor => Self {
                mcp_config_path: project.join(".cursor").join("mcp.json"),
                skill_dir: project.join(".cursor").join("skills"),
                detection_markers: vec![project.join(".cursor")],
                uses_opencode_format: false,
            },
            HostKind::Gemini => Self {
                mcp_config_path: project.join(".gemini").join("mcp.json"),
                skill_dir: project.join(".gemini").join("skills"),
                detection_markers: vec![project.join(".gemini")],
                uses_opencode_format: false,
            },
            HostKind::OpenCode => Self {
                mcp_config_path: project.join("opencode.json"),
                skill_dir: project.join(".opencode").join("skills"),
                detection_markers: vec![project.join("opencode.json"), project.join(".opencode")],
                uses_opencode_format: true,
            },
            HostKind::Codex => Self {
                mcp_config_path: project.join(".codex").join("config.json"),
                skill_dir: project.join(".agents").join("skills"),
                detection_markers: vec![project.join(".codex")],
                uses_opencode_format: false,
            },
        }
    }

    fn for_global_scope(host: HostKind, env: &EnvironmentContext) -> Self {
        let home = &env.home_dir;
        match host {
            HostKind::Claude => {
                #[cfg(target_os = "macos")]
                let mcp_config = home
                    .join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .join("claude_desktop_config.json");
                #[cfg(target_os = "windows")]
                let mcp_config = env
                    .app_data_dir
                    .clone()
                    .join("Claude")
                    .join("claude_desktop_config.json");
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                let mcp_config = env
                    .config_dir
                    .clone()
                    .join("Claude")
                    .join("claude_desktop_config.json");

                let skill_dir = home.join(".claude").join("skills");
                #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
                let mut markers = vec![
                    home.join(".claude"),
                    mcp_config.parent().unwrap_or(home).to_path_buf(),
                ];
                #[cfg(target_os = "macos")]
                markers.push(PathBuf::from("/Applications/Claude.app"));

                Self {
                    mcp_config_path: mcp_config,
                    skill_dir,
                    detection_markers: markers,
                    uses_opencode_format: false,
                }
            }
            HostKind::Cursor => {
                #[cfg(target_os = "macos")]
                let global_storage_mcp = home
                    .join("Library")
                    .join("Application Support")
                    .join("Cursor")
                    .join("User")
                    .join("globalStorage")
                    .join("mcp.json");
                #[cfg(target_os = "windows")]
                let global_storage_mcp = env
                    .app_data_dir
                    .clone()
                    .join("Cursor")
                    .join("User")
                    .join("globalStorage")
                    .join("mcp.json");
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                let global_storage_mcp = env
                    .config_dir
                    .clone()
                    .join("Cursor")
                    .join("User")
                    .join("globalStorage")
                    .join("mcp.json");

                let dot_cursor_mcp = home.join(".cursor").join("mcp.json");
                let mcp_config = if dot_cursor_mcp.exists() || !global_storage_mcp.exists() {
                    dot_cursor_mcp
                } else {
                    global_storage_mcp
                };

                let skill_dir = home.join(".cursor").join("skills");
                let markers = vec![
                    home.join(".cursor"),
                    home.join(".cursor").join("extensions"),
                    home.join("Library")
                        .join("Application Support")
                        .join("Cursor"),
                ];

                Self {
                    mcp_config_path: mcp_config,
                    skill_dir,
                    detection_markers: markers,
                    uses_opencode_format: false,
                }
            }
            HostKind::Gemini => {
                let mcp_config = home
                    .join(".gemini")
                    .join("antigravity-cli")
                    .join("mcp.json");
                let skill_dir = home.join(".gemini").join("skills");
                let markers = vec![
                    home.join(".gemini"),
                    home.join(".gemini").join("antigravity-cli"),
                ];

                Self {
                    mcp_config_path: mcp_config,
                    skill_dir,
                    detection_markers: markers,
                    uses_opencode_format: false,
                }
            }
            HostKind::OpenCode => {
                let config_base = env.config_dir.clone();
                let mcp_config = config_base.join("opencode").join("opencode.json");
                let skill_dir = config_base.join("opencode").join("skills");
                let markers = vec![
                    config_base.join("opencode"),
                    home.join(".opencode"),
                    home.join(".config").join("opencode"),
                ];

                Self {
                    mcp_config_path: mcp_config,
                    skill_dir,
                    detection_markers: markers,
                    uses_opencode_format: true,
                }
            }
            HostKind::Codex => {
                let mcp_config = home.join(".codex").join("config.json");
                let skill_dir = home.join(".agents").join("skills");
                let markers = vec![home.join(".codex")];

                Self {
                    mcp_config_path: mcp_config,
                    skill_dir,
                    detection_markers: markers,
                    uses_opencode_format: false,
                }
            }
        }
    }

    fn is_detected(&self, host: HostKind) -> bool {
        self.has_host_marker() || is_binary_in_path(host.id())
    }

    fn has_host_marker(&self) -> bool {
        self.mcp_config_path.exists() || self.detection_markers.iter().any(|marker| marker.exists())
    }
}

fn is_binary_in_path(bin_name: &str) -> bool {
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths) {
            let candidate = path.join(bin_name);
            if candidate.is_file() {
                return true;
            }
            #[cfg(target_os = "windows")]
            {
                let exe = path.join(format!("{bin_name}.exe"));
                let cmd = path.join(format!("{bin_name}.cmd"));
                let bat = path.join(format!("{bin_name}.bat"));
                if exe.is_file() || cmd.is_file() || bat.is_file() {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(unix)]
fn is_root_context() -> bool {
    unsafe { libc::getuid() == 0 }
}

#[cfg(not(unix))]
fn is_root_context() -> bool {
    false
}

fn resolve_feanorfs_executable() -> Result<PathBuf> {
    if let Ok(override_exe) = std::env::var("FEANORFS_MCP_EXE") {
        return Ok(PathBuf::from(override_exe));
    }
    let current = std::env::current_exe()?;
    current
        .canonicalize()
        .with_context(|| format!("failed to resolve executable {}", current.display()))
}

pub(super) async fn run(_current_dir: &Path, cli: IntegrateCli, json: bool) -> Result<()> {
    let action = cli.action.unwrap_or(IntegrateSubcommand::Install);
    let env = EnvironmentContext::real()?;

    if cli.project.is_none() && is_root_context() && action != IntegrateSubcommand::Status {
        bail!("Refusing to configure user agent hosts from root package-manager context; run `feanorfs integrate` as an ordinary user.");
    }

    let target_hosts = if let Some(ref host_str) = cli.host {
        let Some(host) = HostKind::parse(host_str) else {
            bail!("Unsupported host '{host_str}'. Supported hosts are: claude, cursor, gemini, opencode, codex.");
        };
        vec![host]
    } else {
        HostKind::ALL.to_vec()
    };

    let exe_path = resolve_feanorfs_executable()?;
    let scope_label = if let Some(ref p) = cli.project {
        format!("project ({})", p.display())
    } else {
        "global (user)".to_string()
    };

    let mut report = IntegrationReport {
        scope: scope_label,
        hosts: Vec::new(),
        installed: Vec::new(),
        removed: Vec::new(),
        conflicts: Vec::new(),
        warnings: Vec::new(),
    };

    for host in target_hosts {
        let paths = HostPaths::for_host(host, &env, cli.project.as_deref());
        let detected = cli.project.is_some() || paths.is_detected(host);

        match action {
            IntegrateSubcommand::Status => {
                let mcp_reg_res = check_mcp_registered(&paths);
                let skill_inst = check_skill_installed(&paths);
                let (mcp_registered, conflict_msg) = match mcp_reg_res {
                    Ok(registered) => (registered, None),
                    Err(conflict) => (false, Some(conflict)),
                };

                let status = if !detected {
                    IntegrationStatus::NotDetected
                } else if conflict_msg.is_some() {
                    IntegrationStatus::Conflict
                } else if mcp_registered && skill_inst {
                    IntegrationStatus::Configured
                } else if mcp_registered || skill_inst {
                    IntegrationStatus::PartiallyConfigured
                } else {
                    IntegrationStatus::NotConfigured
                };

                report.hosts.push(HostReport {
                    host,
                    name: host.display_name().to_string(),
                    detected,
                    mcp_config_path: paths.mcp_config_path,
                    mcp_registered,
                    skill_path: paths.skill_dir.join("feanorfs-collaboration"),
                    skill_installed: skill_inst,
                    status,
                    conflict_message: conflict_msg,
                });
            }
            IntegrateSubcommand::Install => {
                if !detected && cli.host.is_none() {
                    report.hosts.push(HostReport {
                        host,
                        name: host.display_name().to_string(),
                        detected: false,
                        mcp_config_path: paths.mcp_config_path,
                        mcp_registered: false,
                        skill_path: paths.skill_dir.join("feanorfs-collaboration"),
                        skill_installed: false,
                        status: IntegrationStatus::NotDetected,
                        conflict_message: None,
                    });
                    continue;
                }

                match install_host_integration(&paths, &exe_path, cli.force).await {
                    Ok((mcp_done, skill_done)) => {
                        let configured_str =
                            format!("{}: MCP={}, Skill={}", host.id(), mcp_done, skill_done);
                        report.installed.push(configured_str);
                        report.hosts.push(HostReport {
                            host,
                            name: host.display_name().to_string(),
                            detected: true,
                            mcp_config_path: paths.mcp_config_path,
                            mcp_registered: true,
                            skill_path: paths.skill_dir.join("feanorfs-collaboration"),
                            skill_installed: true,
                            status: IntegrationStatus::Configured,
                            conflict_message: None,
                        });
                    }
                    Err(err) => {
                        let err_msg = err.to_string();
                        report.conflicts.push(format!("{}: {err_msg}", host.id()));
                        report.hosts.push(HostReport {
                            host,
                            name: host.display_name().to_string(),
                            detected: true,
                            mcp_config_path: paths.mcp_config_path,
                            mcp_registered: false,
                            skill_path: paths.skill_dir.join("feanorfs-collaboration"),
                            skill_installed: false,
                            status: IntegrationStatus::Conflict,
                            conflict_message: Some(err_msg),
                        });
                    }
                }
            }
            IntegrateSubcommand::Uninstall => {
                if !detected
                    && !paths.mcp_config_path.exists()
                    && !paths.skill_dir.join("feanorfs-collaboration").exists()
                {
                    continue;
                }

                match uninstall_host_integration(&paths).await {
                    Ok((mcp_removed, skill_removed)) => {
                        if mcp_removed || skill_removed {
                            report.removed.push(format!(
                                "{}: MCP={}, Skill={}",
                                host.id(),
                                mcp_removed,
                                skill_removed
                            ));
                        }
                        report.hosts.push(HostReport {
                            host,
                            name: host.display_name().to_string(),
                            detected,
                            mcp_config_path: paths.mcp_config_path,
                            mcp_registered: false,
                            skill_path: paths.skill_dir.join("feanorfs-collaboration"),
                            skill_installed: false,
                            status: IntegrationStatus::NotConfigured,
                            conflict_message: None,
                        });
                    }
                    Err(err) => {
                        report
                            .warnings
                            .push(format!("{}: failed uninstall: {err}", host.id()));
                    }
                }
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human_report(&report, action);
    }

    Ok(())
}

fn print_human_report(report: &IntegrationReport, action: IntegrateSubcommand) {
    match action {
        IntegrateSubcommand::Status => {
            println!("FeanorFS Agent Integrations ({})", report.scope);
            println!("------------------------------------------------------------");
            for h in &report.hosts {
                let status_str = match h.status {
                    IntegrationStatus::NotDetected => "not detected",
                    IntegrationStatus::Configured => "configured (MCP + skill)",
                    IntegrationStatus::PartiallyConfigured => "partially configured",
                    IntegrationStatus::NotConfigured => "detected (not configured)",
                    IntegrationStatus::Conflict => "conflict",
                };
                println!("{:<24} {:<24}", h.name, status_str);
                if let Some(ref msg) = h.conflict_message {
                    println!("  Warning: {msg}");
                }
            }
            println!("------------------------------------------------------------");
            println!("Run `feanorfs integrate` to configure all detected hosts.");
        }
        IntegrateSubcommand::Install => {
            println!("FeanorFS Integration Setup ({})", report.scope);
            if report.installed.is_empty() && report.conflicts.is_empty() {
                println!("No installed agent hosts detected on this system.");
                println!("Supported hosts: Codex, Claude, Gemini, OpenCode, Cursor.");
            } else {
                for item in &report.installed {
                    println!("  Registered {item}");
                }
                for item in &report.conflicts {
                    println!("  Conflict: {item}");
                }
            }
        }
        IntegrateSubcommand::Uninstall => {
            println!("FeanorFS Integration Removal ({})", report.scope);
            if report.removed.is_empty() {
                println!("No active FeanorFS integrations found to remove.");
            } else {
                for item in &report.removed {
                    println!("  Removed {item}");
                }
            }
        }
    }
}

fn check_mcp_registered(paths: &HostPaths) -> std::result::Result<bool, String> {
    if !paths.mcp_config_path.exists() {
        return Ok(false);
    }
    let content = match std::fs::read_to_string(&paths.mcp_config_path) {
        Ok(c) => c,
        Err(e) => return Err(format!("cannot read MCP config: {e}")),
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => return Err(format!("invalid JSON in MCP config: {e}")),
    };

    let server_entry = if paths.uses_opencode_format {
        value
            .get("mcp")
            .and_then(|m| m.get("feanorfs"))
            .or_else(|| value.get("mcpServers").and_then(|m| m.get("feanorfs")))
    } else {
        value.get("mcpServers").and_then(|m| m.get("feanorfs"))
    };

    let Some(entry) = server_entry else {
        return Ok(false);
    };

    let cmd = entry_command(entry).map(str::to_owned);
    let Some(cmd) = cmd else {
        return Err("feanorfs MCP entry is missing 'command' field".to_string());
    };

    if is_feanorfs_command(&cmd) {
        Ok(true)
    } else {
        Err(format!(
            "existing 'feanorfs' MCP entry points to external command '{cmd}'"
        ))
    }
}

fn check_skill_installed(paths: &HostPaths) -> bool {
    let skill_root = paths.skill_dir.join("feanorfs-collaboration");
    let skill_md = skill_root.join("SKILL.md");
    let protocol_md = skill_root.join("references").join("protocol.md");
    skill_md.exists() && protocol_md.exists()
}

async fn install_host_integration(
    paths: &HostPaths,
    exe_path: &Path,
    force: bool,
) -> Result<(bool, bool)> {
    reject_unmanaged_skill(paths, force)?;
    let mcp_done = update_mcp_config(paths, exe_path, force)?;
    let skill_done = install_skill_files(paths).await?;
    Ok((mcp_done, skill_done))
}

async fn uninstall_host_integration(paths: &HostPaths) -> Result<(bool, bool)> {
    let mcp_done = remove_mcp_config(paths)?;
    let skill_done = remove_skill_files(paths).await?;
    Ok((mcp_done, skill_done))
}

fn update_mcp_config(paths: &HostPaths, exe_path: &Path, force: bool) -> Result<bool> {
    let config_path = &paths.mcp_config_path;
    let mut root: BTreeMap<String, serde_json::Value> = if config_path.exists() {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("invalid JSON in {}", config_path.display()))?
    } else {
        BTreeMap::new()
    };

    let exe_str = exe_path
        .to_str()
        .with_context(|| format!("executable path is not valid UTF-8: {}", exe_path.display()))?;
    let uses_opencode_entry = paths.uses_opencode_format
        && (root.contains_key("mcp") || !root.contains_key("mcpServers"));
    let config_key = if uses_opencode_entry {
        "mcp"
    } else {
        "mcpServers"
    };
    let servers = root
        .entry(config_key.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let serde_json::Value::Object(map) = servers else {
        bail!(
            "'{config_key}' in {} is not a JSON object",
            config_path.display()
        );
    };
    reject_foreign_entry(map, config_path, force)?;

    let new_entry = if uses_opencode_entry {
        // opencode validates `mcp.<name>` as {type:"local"|"remote", enabled,
        // command:[argv..]}; the legacy {type:"stdio", command:<str>, args}
        // shape fails its schema check and bricks the whole config.
        serde_json::json!({
            "type": "local",
            "enabled": true,
            "command": [exe_str, "mcp"]
        })
    } else {
        serde_json::json!({ "command": exe_str, "args": ["mcp"] })
    };
    map.insert("feanorfs".to_string(), new_entry);

    let rendered = serde_json::to_string_pretty(&root)?;
    atomic_write_file(config_path, rendered.as_bytes())?;

    Ok(true)
}

fn remove_mcp_config(paths: &HostPaths) -> Result<bool> {
    let config_path = &paths.mcp_config_path;
    if !config_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut root: BTreeMap<String, serde_json::Value> = serde_json::from_str(&content)
        .with_context(|| format!("invalid JSON in {}", config_path.display()))?;

    let mut changed = false;

    if let Some(serde_json::Value::Object(map)) = root.get_mut("mcpServers") {
        changed |= remove_owned_entry(map, config_path)?;
    }
    if let Some(serde_json::Value::Object(map)) = root.get_mut("mcp") {
        changed |= remove_owned_entry(map, config_path)?;
    }

    if changed {
        let rendered = serde_json::to_string_pretty(&root)?;
        atomic_write_file(config_path, rendered.as_bytes())?;
    }

    Ok(changed)
}

async fn install_skill_files(paths: &HostPaths) -> Result<bool> {
    let skill_root = paths.skill_dir.join("feanorfs-collaboration");
    let references_dir = skill_root.join("references");
    let agents_dir = skill_root.join("agents");

    tokio::fs::create_dir_all(&references_dir)
        .await
        .with_context(|| format!("failed to create {}", references_dir.display()))?;
    tokio::fs::create_dir_all(&agents_dir)
        .await
        .with_context(|| format!("failed to create {}", agents_dir.display()))?;

    atomic_write_file(&skill_root.join("SKILL.md"), SKILL_MD.as_bytes())?;
    atomic_write_file(&references_dir.join("protocol.md"), PROTOCOL_MD.as_bytes())?;
    atomic_write_file(&agents_dir.join("openai.yaml"), OPENAI_YAML.as_bytes())?;
    atomic_write_file(&skill_root.join(MANAGED_MARKER), b"managed-by:feanorfs\n")?;

    Ok(true)
}

async fn remove_skill_files(paths: &HostPaths) -> Result<bool> {
    let skill_root = paths.skill_dir.join("feanorfs-collaboration");
    if !skill_root.exists() {
        return Ok(false);
    }

    let marker = skill_root.join(MANAGED_MARKER);
    if marker.exists() {
        tokio::fs::remove_dir_all(&skill_root)
            .await
            .with_context(|| format!("failed to remove {}", skill_root.display()))?;
        return Ok(true);
    }

    Ok(false)
}

pub(super) async fn install_detected_hosts_quiet() -> Result<()> {
    if is_root_context() {
        return Ok(());
    }
    let env = match EnvironmentContext::real() {
        Ok(env) => env,
        Err(_) => return Ok(()),
    };
    let exe_path = match resolve_feanorfs_executable() {
        Ok(exe) => exe,
        Err(_) => return Ok(()),
    };
    for &host in HostKind::ALL {
        let paths = HostPaths::for_host(host, &env, None);
        if paths.is_detected(host) {
            let _ = install_host_integration(&paths, &exe_path, false).await;
        }
    }
    Ok(())
}

fn is_feanorfs_command(command: &str) -> bool {
    Path::new(command)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("feanorfs"))
}

fn entry_command(entry: &serde_json::Value) -> Option<&str> {
    match entry.get("command") {
        Some(serde_json::Value::String(text)) => Some(text),
        Some(serde_json::Value::Array(items)) => items.first().and_then(|item| item.as_str()),
        _ => None,
    }
}

fn reject_foreign_entry(
    map: &serde_json::Map<String, serde_json::Value>,
    config_path: &Path,
    force: bool,
) -> Result<()> {
    let Some(existing) = map.get("feanorfs") else {
        return Ok(());
    };
    let existing_command = entry_command(existing).unwrap_or("");
    if !is_feanorfs_command(existing_command) && !force {
        bail!(
            "existing 'feanorfs' entry in {} points to '{}'; use --force to overwrite",
            config_path.display(),
            existing_command
        );
    }
    Ok(())
}

fn remove_owned_entry(
    map: &mut serde_json::Map<String, serde_json::Value>,
    config_path: &Path,
) -> Result<bool> {
    let Some(entry) = map.get("feanorfs") else {
        return Ok(false);
    };
    let command = entry_command(entry).unwrap_or("");
    if !is_feanorfs_command(command) {
        bail!(
            "existing 'feanorfs' entry in {} points to '{}'; refusing to remove it",
            config_path.display(),
            command
        );
    }
    map.remove("feanorfs");
    Ok(true)
}

fn reject_unmanaged_skill(paths: &HostPaths, force: bool) -> Result<()> {
    let skill_root = paths.skill_dir.join("feanorfs-collaboration");
    if force || !skill_root.exists() || skill_root.join(MANAGED_MARKER).exists() {
        return Ok(());
    }
    bail!(
        "existing skill directory {} is not managed by FeanorFS; use --force to overwrite",
        skill_root.display()
    )
}

fn atomic_write_file(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("integration file path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;

    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;

    file.write_all(content)?;
    file.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn install_and_uninstall_global_scope_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let env = EnvironmentContext::mock(temp.path());
        let fake_exe = temp.path().join("bin").join("feanorfs");

        for &host in HostKind::ALL {
            let paths = HostPaths::for_host(host, &env, None);
            assert!(!check_skill_installed(&paths));

            let (mcp_done, skill_done) = install_host_integration(&paths, &fake_exe, false)
                .await
                .unwrap();
            assert!(mcp_done);
            assert!(skill_done);
            assert!(check_skill_installed(&paths));
            assert!(check_mcp_registered(&paths).unwrap());

            let (mcp_second, skill_second) = install_host_integration(&paths, &fake_exe, false)
                .await
                .unwrap();
            assert!(mcp_second);
            assert!(skill_second);

            let (mcp_removed, skill_removed) = uninstall_host_integration(&paths).await.unwrap();
            assert!(mcp_removed);
            assert!(skill_removed);
            assert!(!check_skill_installed(&paths));
            assert!(!check_mcp_registered(&paths).unwrap());
        }
    }

    #[tokio::test]
    async fn project_scope_configures_explicit_directory() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("my-project");
        let env = EnvironmentContext::mock(temp.path());
        let fake_exe = temp.path().join("bin").join("feanorfs");

        for &host in HostKind::ALL {
            let paths = HostPaths::for_host(host, &env, Some(&project_dir));
            assert!(paths.mcp_config_path.starts_with(&project_dir));
            assert!(paths.skill_dir.starts_with(&project_dir));

            let (mcp_done, skill_done) = install_host_integration(&paths, &fake_exe, false)
                .await
                .unwrap();
            assert!(mcp_done);
            assert!(skill_done);
            assert!(paths.mcp_config_path.exists());
            assert!(paths
                .skill_dir
                .join("feanorfs-collaboration")
                .join("SKILL.md")
                .exists());
        }
    }

    #[test]
    fn host_skill_directories_match_supported_host_conventions() {
        let temp = tempfile::tempdir().unwrap();
        let env = EnvironmentContext::mock(temp.path());
        let project = temp.path().join("project");

        let cases = [
            (HostKind::Claude, ".claude/skills", ".claude/skills"),
            (HostKind::Cursor, ".cursor/skills", ".cursor/skills"),
            (HostKind::Gemini, ".gemini/skills", ".gemini/skills"),
            (
                HostKind::OpenCode,
                ".config/opencode/skills",
                ".opencode/skills",
            ),
            (HostKind::Codex, ".agents/skills", ".agents/skills"),
        ];

        for (host, global_suffix, project_suffix) in cases {
            let global = HostPaths::for_host(host, &env, None);
            assert_eq!(global.skill_dir, temp.path().join(global_suffix));

            let scoped = HostPaths::for_host(host, &env, Some(&project));
            assert_eq!(scoped.skill_dir, project.join(project_suffix));
        }
    }

    #[test]
    fn shared_agents_skill_directory_does_not_detect_codex() {
        let temp = tempfile::tempdir().unwrap();
        let env = EnvironmentContext::mock(temp.path());
        std::fs::create_dir_all(temp.path().join(".agents").join("skills")).unwrap();

        let paths = HostPaths::for_host(HostKind::Codex, &env, None);
        assert!(!paths.has_host_marker());
    }

    #[tokio::test]
    async fn preserves_unrelated_mcp_entries_and_flags_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let env = EnvironmentContext::mock(temp.path());
        let fake_exe = temp.path().join("bin").join("feanorfs");

        let paths = HostPaths::for_host(HostKind::Claude, &env, None);

        tokio::fs::create_dir_all(paths.mcp_config_path.parent().unwrap())
            .await
            .unwrap();
        let initial_json = serde_json::json!({
            "mcpServers": {
                "sqlite": {
                    "command": "sqlite-mcp",
                    "args": ["--db", "test.db"]
                }
            },
            "unrelatedSetting": true
        });
        std::fs::write(
            &paths.mcp_config_path,
            serde_json::to_string_pretty(&initial_json).unwrap(),
        )
        .unwrap();

        install_host_integration(&paths, &fake_exe, false)
            .await
            .unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&paths.mcp_config_path).unwrap())
                .unwrap();
        assert!(updated.get("unrelatedSetting").unwrap().as_bool().unwrap());
        assert!(updated["mcpServers"].get("sqlite").is_some());
        assert_eq!(
            updated["mcpServers"]["feanorfs"]["command"]
                .as_str()
                .unwrap(),
            fake_exe.to_str().unwrap()
        );

        let conflicting_json = serde_json::json!({
            "mcpServers": {
                "feanorfs": {
                    "command": "python /foreign/tool.py",
                    "args": []
                }
            }
        });
        std::fs::write(
            &paths.mcp_config_path,
            serde_json::to_string_pretty(&conflicting_json).unwrap(),
        )
        .unwrap();

        let err = install_host_integration(&paths, &fake_exe, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("use --force to overwrite"));

        install_host_integration(&paths, &fake_exe, true)
            .await
            .unwrap();
        let forced: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&paths.mcp_config_path).unwrap())
                .unwrap();
        assert_eq!(
            forced["mcpServers"]["feanorfs"]["command"]
                .as_str()
                .unwrap(),
            fake_exe.to_str().unwrap()
        );
    }

    #[tokio::test]
    async fn uninstall_preserves_unmanaged_skill_directory() {
        let temp = tempfile::tempdir().unwrap();
        let env = EnvironmentContext::mock(temp.path());
        let paths = HostPaths::for_host(HostKind::Cursor, &env, None);
        let skill_root = paths.skill_dir.join("feanorfs-collaboration");
        std::fs::create_dir_all(&skill_root).unwrap();
        std::fs::write(skill_root.join("SKILL.md"), "user-owned\n").unwrap();

        assert!(!remove_skill_files(&paths).await.unwrap());
        assert_eq!(
            std::fs::read_to_string(skill_root.join("SKILL.md")).unwrap(),
            "user-owned\n"
        );
    }

    #[tokio::test]
    async fn install_requires_force_for_unmanaged_skill_directory() {
        let temp = tempfile::tempdir().unwrap();
        let env = EnvironmentContext::mock(temp.path());
        let paths = HostPaths::for_host(HostKind::Cursor, &env, None);
        let skill_root = paths.skill_dir.join("feanorfs-collaboration");
        std::fs::create_dir_all(&skill_root).unwrap();
        std::fs::write(skill_root.join("SKILL.md"), "user-owned\n").unwrap();
        let executable = temp.path().join("feanorfs");

        let error = install_host_integration(&paths, &executable, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("use --force to overwrite"));
        assert!(!paths.mcp_config_path.exists());
        assert_eq!(
            std::fs::read_to_string(skill_root.join("SKILL.md")).unwrap(),
            "user-owned\n"
        );

        install_host_integration(&paths, &executable, true)
            .await
            .unwrap();
        assert!(skill_root.join(MANAGED_MARKER).exists());
        assert_eq!(
            std::fs::read_to_string(skill_root.join("SKILL.md")).unwrap(),
            SKILL_MD
        );
    }

    #[test]
    fn uninstall_refuses_foreign_mcp_entry() {
        let temp = tempfile::tempdir().unwrap();
        let env = EnvironmentContext::mock(temp.path());
        let paths = HostPaths::for_host(HostKind::Cursor, &env, None);
        std::fs::create_dir_all(paths.mcp_config_path.parent().unwrap()).unwrap();
        let config = serde_json::json!({
            "mcpServers": {
                "feanorfs": {
                    "command": "python",
                    "args": ["foreign-feanorfs.py"]
                }
            }
        });
        std::fs::write(
            &paths.mcp_config_path,
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .unwrap();

        let error = remove_mcp_config(&paths).unwrap_err();
        assert!(error.to_string().contains("refusing to remove it"));
        let preserved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&paths.mcp_config_path).unwrap()).unwrap();
        assert_eq!(preserved, config);
    }
}
