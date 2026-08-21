feanorfs_test_support::isolate_test_process!();

// Documentation/skill/API conformance.
//
// The frozen operation matrix (`operation_matrix.rs`) pins operations on
// code surfaces. This suite pins the public *text* surfaces: every JSON
// example in the docs and the collaboration skill must still decode through
// the real wire decoders (`parse_agent_message`, `parse_integrator_profile`,
// `parse_work_profile`, resolution contract decode), schema versions and
// bounds must agree with the wire constants, command names must come from
// the canonical matrix, and no text may imply capabilities FeanorFS
// explicitly does not have (auto-merge, process sandboxing, plaintext hub
// task semantics, Git push/pull ownership). Stale public examples fail CI.

use std::path::{Path, PathBuf};

use feanorfs_common::agent_contract::{
    encode_agent_message, parse_agent_message, AgentMessageKind, AgentMessagePayload,
    AGENT_INBOX_DEFAULT_LIMIT, AGENT_INBOX_MAX_LIMIT, AGENT_MESSAGE_DISCRIMINATOR,
    AGENT_MESSAGE_MAX_BODY_BYTES, AGENT_MESSAGE_MAX_ENCODED_BYTES,
    CONTINUOUS_STATUS_SCHEMA_VERSION,
};
use feanorfs_common::hub_contract::SUPPORTED_FORMAT_VERSION;
use feanorfs_common::integrator_contract::{
    encode_integrator_profile, parse_integrator_profile, IntegratorProfile,
    INTEGRATOR_PROFILE_DISCRIMINATOR,
};
use feanorfs_common::resolution_contract::{
    resolution_fixtures, validate_resolution_job, validate_resolution_result, ResolutionJob,
    ResolutionResult, RESOLUTION_MAX_ATTEMPT, RESOLUTION_SCHEMA_VERSION,
};
use feanorfs_common::work_contract::{
    encode_work_profile, parse_work_profile, WorkIntentProfile, WorkProfile,
    WORK_MAX_PROFILE_BYTES, WORK_PROFILE_DISCRIMINATOR, WORK_SCHEMA_VERSION,
};

/// Every public doc/skill text surface. Review/sweep notes under `docs/` are
/// internal audit artifacts, not public contract surfaces, so they are not
/// validated here.
const PUBLIC_DOC_FILES: &[&str] = &[
    "docs/agent-api.md",
    "docs/agent-communication.md",
    "docs/usage.md",
    "docs/threat-model.md",
    "skills/feanorfs-collaboration/SKILL.md",
    "skills/feanorfs-collaboration/references/protocol.md",
];

/// Files carrying complete wire-format examples (`ffmsg1:`/`ffint1:`/`ffwork1:`
/// lines). SKILL.md references profiles but never shows complete JSON, so it
/// is excluded from extraction.
const WIRE_EXAMPLE_FILES: &[&str] = &[
    "docs/agent-api.md",
    "docs/agent-communication.md",
    "skills/feanorfs-collaboration/references/protocol.md",
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("client crate lives one level under the workspace root")
        .to_path_buf()
}

fn surface(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel))
        .unwrap_or_else(|error| panic!("surface {rel} unreadable: {error}"))
}

fn read_doc(rel: &str) -> String {
    surface(&workspace_root(), rel)
}

fn hex64() -> String {
    "a".repeat(64)
}

fn hex32() -> String {
    "0123456789abcdef0123456789abcdef".to_string()
}

/// Replaces every documented placeholder value with canonical fixture values
/// so the example decodes through the real parsers. Key order, spacing, and
/// `null` markers are preserved byte-for-byte; only quoted values change.
fn substitute_placeholders(json: &str) -> String {
    json.replace("\"<64-hex>\"", &format!("\"{}\"", hex64()))
        .replace("\"<32-hex>\"", &format!("\"{}\"", hex32()))
        .replace("\"<observed-workspace-head>\"", &format!("\"{}\"", hex64()))
        .replace("\"<64-hex-plaintext-hash>\"", &format!("\"{}\"", hex64()))
        .replace('…', &hex64())
}

#[test]
fn canonical_fixtures_roundtrip_through_the_real_decoders() {
    // ffmsg1, ffint1, ffwork1, ResolutionJob, and ResolutionResult fixtures.
    // Each is built with the real encoder and must decode
    // back through the real parser unchanged.
    assert_eq!(AGENT_MESSAGE_DISCRIMINATOR, "ffmsg1");
    assert_eq!(INTEGRATOR_PROFILE_DISCRIMINATOR, "ffint1");
    assert_eq!(WORK_PROFILE_DISCRIMINATOR, "ffwork1");

    let message = AgentMessagePayload {
        to: "mac-test".into(),
        kind: AgentMessageKind::Request,
        body: "Run iOS simulator tests".into(),
        about_snapshot: hex64(),
        reply_to: None,
    };
    let encoded = encode_agent_message(&message).unwrap();
    assert!(encoded.starts_with("ffmsg1:"));
    assert_eq!(parse_agent_message(&encoded), Some(message));

    let assignment = IntegratorProfile::Assignment {
        assignment_id: hex32(),
        attempt: 0,
        selected: "agent-b".into(),
        about_snapshot: hex64(),
        roster_fingerprint: hex64(),
        neutral_integrator: true,
        task: "Integrate parser implementation and tests".into(),
    };
    let encoded = encode_integrator_profile(&assignment).unwrap();
    assert!(encoded.starts_with("ffint1:"));
    assert_eq!(parse_integrator_profile(&encoded), Some(assignment));

    let work = WorkProfile::WorkIntent(WorkIntentProfile {
        task_id: "parser-impl".into(),
        agent: "linux-dev".into(),
        sequence: 1,
        causal_base: None,
        coordinator: Some("human".into()),
        paths: vec!["src/parser.rs".into(), "tests/parser.rs".into()],
        concerns: vec!["parser behavior".into()],
        dependencies: vec![],
        capabilities: vec!["rust".into()],
    });
    let encoded = encode_work_profile(&work).unwrap();
    assert!(encoded.starts_with("ffwork1:"));
    assert_eq!(parse_work_profile(&encoded), Some(work));

    let job = resolution_fixtures::job();
    validate_resolution_job(&job).unwrap();
    let result = resolution_fixtures::result();
    validate_resolution_result(&result).unwrap();
    let human = resolution_fixtures::human_result();
    validate_resolution_result(&human).unwrap();
}

/// One complete wire example lifted from a doc line.
struct WireExample {
    file: &'static str,
    line: usize,
    discriminator: &'static str,
    json: String,
}

fn wire_examples() -> Vec<WireExample> {
    let mut examples = Vec::new();
    for file in WIRE_EXAMPLE_FILES {
        for (idx, line) in read_doc(file).lines().enumerate() {
            for discriminator in ["ffmsg1", "ffint1", "ffwork1"] {
                let prefix = format!("{discriminator}:");
                if let Some(rest) = line.strip_prefix(&prefix) {
                    examples.push(WireExample {
                        file,
                        line: idx + 1,
                        discriminator,
                        json: rest.to_string(),
                    });
                }
            }
        }
    }
    examples
}

#[test]
fn doc_wire_examples_decode_through_the_real_decoders() {
    let examples = wire_examples();
    assert!(
        examples.len() >= 8,
        "expected at least the 8 documented wire examples, found {}: a doc edit removed one",
        examples.len()
    );
    for example in examples {
        let substituted = substitute_placeholders(&example.json);
        // The parsers expect the full wire body (`discriminator:` + canonical
        // compact JSON), which is exactly what the docs show.
        let body = format!("{}:{substituted}", example.discriminator);
        let parsed = match example.discriminator {
            "ffmsg1" => parse_agent_message(&body).map(|payload| payload.kind.as_str().to_string()),
            "ffint1" => parse_integrator_profile(&body)
                .map(|profile| match profile {
                    IntegratorProfile::Assignment { .. } => "assignment",
                    IntegratorProfile::Accepted { .. } => "accepted",
                    IntegratorProfile::Result { .. } => "result",
                    IntegratorProfile::Blocked { .. } => "blocked",
                })
                .map(str::to_string),
            "ffwork1" => parse_work_profile(&body).map(|profile| profile.type_name().to_string()),
            other => panic!("unknown discriminator {other}"),
        };
        assert!(
            parsed.is_some(),
            "{}:{} example does not decode through the real parser: {}",
            example.file,
            example.line,
            body
        );
    }
}

/// Extracts the first fenced ```json block after `heading`.
fn fenced_json_after(doc: &str, heading: &str) -> Option<String> {
    let start = doc.find(heading)?;
    let rest = &doc[start..];
    let fence = rest.find("```json")?;
    let after = &rest[fence + "```json".len()..];
    let end = after.find("```")?;
    Some(after[..end].trim().to_string())
}

#[test]
fn resolution_doc_examples_decode_and_match_the_canonical_fixtures() {
    // The documented `ResolutionJob`/`ResolutionResult` JSON must decode
    // through the real contract validation and equal the canonical fixtures
    // field-for-field: the docs were generated from the fixtures, so either
    // side drifting alone fails CI.
    let api = read_doc("docs/agent-api.md");

    let job_json =
        fenced_json_after(&api, "### `ResolutionJob`").expect("documented ResolutionJob example");
    let job: ResolutionJob =
        serde_json::from_str(&job_json).expect("documented ResolutionJob must parse");
    validate_resolution_job(&job).expect("documented ResolutionJob must validate");
    assert_eq!(
        serde_json::to_value(&job).unwrap(),
        serde_json::to_value(resolution_fixtures::job()).unwrap(),
        "documented ResolutionJob drifted from the canonical fixture"
    );

    let result_json = fenced_json_after(&api, "### `ResolutionResult`")
        .expect("documented ResolutionResult example");
    let fixture = resolution_fixtures::result();
    let fixture_hash = fixture
        .candidate
        .as_ref()
        .expect("fixture carries a candidate")
        .hash
        .clone();
    let substituted = result_json.replace(
        "\"<64-hex-plaintext-hash>\"",
        &format!("\"{fixture_hash}\""),
    );
    let result: ResolutionResult =
        serde_json::from_str(&substituted).expect("documented ResolutionResult must parse");
    validate_resolution_result(&result).expect("documented ResolutionResult must validate");
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        serde_json::to_value(&fixture).unwrap(),
        "documented ResolutionResult drifted from the canonical fixture"
    );
}

#[test]
fn schema_versions_in_docs_match_the_wire_constants() {
    // Every serialized `schema_version` in the public docs is pinned to the
    // current wire schemas; a version bump must land in docs and constants
    // in the same change.
    for file in PUBLIC_DOC_FILES {
        for (idx, line) in read_doc(file).lines().enumerate() {
            let Some((_, rest)) = line.split_once("\"schema_version\":") else {
                continue;
            };
            let value: String = rest
                .trim_start()
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect();
            assert_eq!(
                value, "1",
                "{file}:{} documents schema_version {value}; every current wire contract is schema 1",
                idx + 1
            );
        }
    }
    assert_eq!(WORK_SCHEMA_VERSION, 1);
    assert_eq!(RESOLUTION_SCHEMA_VERSION, 1);
    assert_eq!(CONTINUOUS_STATUS_SCHEMA_VERSION, 1);
    assert_eq!(SUPPORTED_FORMAT_VERSION, 3);

    // The transport envelope and the runner child invocation also pin their
    // versions in prose.
    let protocol = read_doc("skills/feanorfs-collaboration/references/protocol.md");
    assert!(
        protocol.contains("format-v3"),
        "protocol reference must pin the format-v3 envelope"
    );
    let skill = read_doc("skills/feanorfs-collaboration/SKILL.md");
    assert!(
        skill.contains("schema_version: 1"),
        "skill runner section must pin schema_version: 1"
    );
}

#[test]
fn documented_bounds_match_the_wire_bounds() {
    // The documented limits are the published contract; the constants are
    // canonical. Docs and constants must agree in both directions.
    assert_eq!(AGENT_MESSAGE_MAX_BODY_BYTES, 8 * 1024);
    assert_eq!(AGENT_MESSAGE_MAX_ENCODED_BYTES, 64 * 1024);
    assert_eq!(AGENT_INBOX_DEFAULT_LIMIT, 50);
    assert_eq!(AGENT_INBOX_MAX_LIMIT, 1000);
    assert_eq!(WORK_MAX_PROFILE_BYTES, AGENT_MESSAGE_MAX_BODY_BYTES);
    assert_eq!(RESOLUTION_MAX_ATTEMPT, 10_000);

    let protocol = read_doc("skills/feanorfs-collaboration/references/protocol.md");
    let communication = read_doc("docs/agent-communication.md");
    let api = read_doc("docs/agent-api.md");
    let skill = read_doc("skills/feanorfs-collaboration/SKILL.md");

    assert!(
        protocol.contains("at most 8 KiB"),
        "8 KiB body bound in protocol"
    );
    assert!(skill.contains("maximum 8 KiB"), "8 KiB body bound in skill");
    assert!(api.contains("64 KiB"), "64 KiB envelope bound in agent-api");
    assert!(
        protocol.contains("default 50"),
        "inbox default limit in protocol"
    );
    assert!(
        protocol.contains("maximum 1000"),
        "inbox maximum limit in protocol"
    );
    assert!(
        communication.contains("10 000"),
        "10 000 traversal scan cap in agent-communication"
    );
    assert!(
        protocol.contains("10 000"),
        "10 000 traversal scan cap in protocol"
    );
}

/// Canonical CLI command forms from the frozen operation matrix
/// (`operation_matrix.rs`). `sync`, `events`, `agent runner`, and `mcp` are
/// documented lifecycle/operator surfaces, not matrix operations; the skill
/// may reference them only as operator context.
const CANONICAL_COMMANDS: &[&str] = &[
    "agent spawn",
    "agent status",
    "agent refresh",
    "agent land",
    "agent clean",
    "agent send",
    "agent inbox",
    "agent run",
    "agent work propose",
    "agent work decide",
    "agent work amend",
    "agent work yield",
    "agent work settle",
    "agent work complete",
    "agent work block",
    "agent work status",
    "agent resolution prepare",
    "agent resolution status",
    "agent resolution submit",
    "agent resolution apply",
    "agent resolution materialize",
    "agent resolution put",
    "agent resolution answer",
    "agent resolution defer",
    "agent resolution protocol-status",
    "agent resolution assign",
    "agent resolution reply",
    "agent resolution revoke",
    "agent resolution publish-answer",
    "agent integrator assign",
    "agent integrator status",
    "agent integrator revoke",
    "agent integrator resume",
    "conflicts",
    "conflicts keep",
    "conflicts materialize",
    "log",
    "undo",
];

#[test]
fn matrix_cli_commands_remain_documented_on_the_public_surfaces() {
    let api = read_doc("docs/agent-api.md");
    let usage = read_doc("docs/usage.md");
    let skill = read_doc("skills/feanorfs-collaboration/SKILL.md");
    for command in CANONICAL_COMMANDS {
        assert!(
            api.contains(command) || usage.contains(command) || skill.contains(command),
            "canonical matrix command `{command}` is documented on no public surface"
        );
    }
}

/// Collects every `feanorfs …` invocation from the skill and its protocol
/// reference: the first up-to-four non-flag words after `feanorfs`.
fn skill_command_fragments(contents: &[String]) -> Vec<String> {
    let mut fragments = Vec::new();
    for content in contents {
        let mut rest = content.as_str();
        while let Some(pos) = rest.find("feanorfs ") {
            rest = &rest[pos + "feanorfs ".len()..];
            let mut parts = Vec::new();
            for word in rest.split_whitespace() {
                let trimmed = word.trim_matches(|c| matches!(c, '`' | '(' | ')' | ',' | ';' | '.'));
                if trimmed.starts_with('-') || trimmed.is_empty() {
                    continue;
                }
                parts.push(trimmed.to_string());
                if parts.len() == 4 {
                    break;
                }
            }
            if !parts.is_empty() {
                fragments.push(parts.join(" "));
            }
        }
    }
    fragments
}

#[test]
fn skill_commands_are_canonical_matrix_operations() {
    let skill = read_doc("skills/feanorfs-collaboration/SKILL.md");
    let protocol = read_doc("skills/feanorfs-collaboration/references/protocol.md");
    // `sync` and `events` are the documented operator/lifecycle surfaces the
    // skill legitimately references ("never run `feanorfs sync` …").
    const OPERATOR_SURFACES: &[&str] = &["sync", "events", "mcp", "agent runner"];
    let allowed = |fragment: &str| {
        CANONICAL_COMMANDS.iter().any(|c| fragment.starts_with(c))
            || OPERATOR_SURFACES.iter().any(|c| fragment.starts_with(c))
    };

    for fragment in skill_command_fragments(&[skill, protocol]) {
        assert!(
            allowed(&fragment),
            "skill references a non-matrix command: `feanorfs {fragment}`"
        );
    }
}

fn backticked_tokens_in(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn paragraph_containing<'a>(doc: &'a str, anchor: &str) -> &'a str {
    doc.split("\n\n")
        .find(|paragraph| paragraph.contains(anchor))
        .unwrap_or_else(|| panic!("no paragraph containing {anchor:?}"))
}

fn assert_same_set(label: &str, actual: &mut Vec<String>, expected: &[&str]) {
    actual.sort();
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(
        *actual, expected,
        "{label}: documented set drifted from the wire enum set"
    );
}

#[test]
fn documented_state_transition_sets_match_the_wire_enums() {
    // The docs enumerate the closed transition sets; the lists are the
    // frozen contract shared by every surface. Extracting them from the
    // prose and comparing against the canonical enum sets catches drift on
    // either side.

    // Message kinds (agent-communication.md "Message kinds" table).
    let communication = read_doc("docs/agent-communication.md");
    let section_start = communication
        .find("## Message kinds")
        .expect("message kinds section");
    let section_end = communication[section_start..]
        .find("\n## ")
        .map(|end| section_start + end)
        .unwrap_or(communication.len());
    let mut kinds = communication[section_start..section_end]
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("| `") {
                return None;
            }
            trimmed.split('`').nth(1).map(str::to_string)
        })
        .collect();
    assert_same_set(
        "message kinds",
        &mut kinds,
        &["request", "status", "result", "blocked"],
    );

    // Work profile variant tags and decision kinds (agent-api.md). The
    // "Variant tags:" paragraph wraps across lines, so it is extracted
    // paragraph-wide with whitespace normalized before tokenizing.
    let api = read_doc("docs/agent-api.md");
    let variant_paragraph = paragraph_containing(&api, "Variant tags:")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut work_tags: Vec<String> = variant_paragraph
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|token| token.starts_with("work_"))
        .map(str::to_string)
        .collect();
    assert_same_set(
        "work variant tags",
        &mut work_tags,
        &[
            "work_intent",
            "work_decision",
            "work_amendment",
            "work_yield",
            "work_settled",
            "work_completed",
            "work_blocked",
            "work_superseded",
        ],
    );
    let after_kind = variant_paragraph
        .split("`kind` ")
        .nth(1)
        .expect("work decision kind list");
    let mut decision_kinds: Vec<String> = after_kind
        .split(')')
        .next()
        .expect("decision list terminator")
        .split(" | ")
        .map(str::to_string)
        .collect();
    assert_same_set(
        "work decision kinds",
        &mut decision_kinds,
        &["accept", "reject", "narrow", "order", "accept_overlap"],
    );

    // Resolution outcomes and human reasons (agent-api.md). The prose
    // backticks the field name, so the anchor spans the backticks.
    let outcome_paragraph = paragraph_containing(&api, "is a closed set");
    let mut outcomes: Vec<String> = backticked_tokens_in(
        outcome_paragraph
            .split(". ")
            .next()
            .expect("outcome sentence"),
    )
    .into_iter()
    .filter(|token| token != "outcome")
    .collect();
    assert_same_set(
        "resolution outcomes",
        &mut outcomes,
        &[
            "candidate_ready",
            "no_change_required",
            "blocked",
            "requires_human",
            "failed",
            "stale",
        ],
    );
    let reasons_open = outcome_paragraph.find("(`").expect("human reasons list");
    let reasons_close = reasons_open
        + outcome_paragraph[reasons_open..]
            .find(')')
            .expect("human reasons list terminator");
    let mut human_reasons =
        backticked_tokens_in(&outcome_paragraph[reasons_open + 1..reasons_close]);
    assert_same_set(
        "human resolution reasons",
        &mut human_reasons,
        &[
            "semantic_ambiguity",
            "unavoidable_data_loss",
            "missing_or_auth_failed_leg",
            "security_compatibility_boundary_change",
            "required_verification_unavailable",
            "indeterminate_ownership",
            "bounded_resolver_exhaustion",
            "unsupported_size_safety_bound",
            "explicit_product_decision",
        ],
    );

    // Continuous reconciliation phases (agent-api.md).
    let phase_paragraph = paragraph_containing(&api, "is one of `starting`");
    let mut phases: Vec<String> =
        backticked_tokens_in(phase_paragraph.split(". ").next().expect("phase sentence"))
            .into_iter()
            .filter(|token| token != "phase")
            .collect();
    assert_same_set(
        "continuous phases",
        &mut phases,
        &[
            "starting",
            "idle",
            "local_dirty",
            "reconciling_local",
            "refreshing_remote",
            "offline",
            "needs_attention",
            "stopping",
        ],
    );
}

/// Capability phrases whose mere presence claims a behavior FeanorFS does not
/// have. A paragraph may mention them only while denying them (never/not/…):
/// the docs carefully do ("never auto-merges", "not a sandbox"), and any
/// affirmative claim fails CI.
const FORBIDDEN_CAPABILITY_TOKENS: &[&str] = &[
    // Auto-merge claims.
    "auto-merge",
    "auto merge",
    "auto-merged",
    "auto merged",
    "automatically merge",
    "automatically merged",
    "merged automatically",
    // FeanorFS process/agent sandboxing claims.
    "process sandboxing",
    "sandbox the process",
    "sandboxed execution",
    "runs in a sandbox",
    "sandbox for",
    // Plaintext hub/task semantics.
    "plaintext routing",
    "plaintext index",
    "plaintext bodies",
    "plaintext body",
    "plaintext task",
    "plaintext tasks",
    "in plaintext",
    // Git push/pull ownership.
    "git push",
    "git pull",
    "git push/pull",
    "git-shaped",
    "feanorfs push",
    "feanorfs pull",
    "push and pull",
    "pushing and pulling",
];

const DENIAL_TOKENS: &[&str] = &[
    "never",
    "not",
    "without",
    "doesn",
    "cannot",
    "can't",
    "won't",
    "isn't",
    "aren't",
    "unavailable",
    "denied",
    "rejects",
    "reject",
    "refused",
    " no ",
];

#[test]
fn forbidden_semantics_never_appear_as_product_claims() {
    // A paragraph boundary is used because the docs routinely split a denial
    // across wrapped lines ("… never sees signal bodies, routing, or\n
    // snapshot context in plaintext."). Check at paragraph granularity so a
    // denial on the previous wrapped line still protects the sentence.
    for file in PUBLIC_DOC_FILES {
        let content = read_doc(file);
        for (paragraph_index, paragraph) in content.split("\n\n").enumerate() {
            let lower = paragraph.to_lowercase();
            let claimed: Vec<&str> = FORBIDDEN_CAPABILITY_TOKENS
                .iter()
                .copied()
                .filter(|token| lower.contains(token))
                .collect();
            if claimed.is_empty() {
                continue;
            }
            let denied = DENIAL_TOKENS.iter().any(|token| lower.contains(token));
            assert!(
                denied,
                "{file} paragraph {} claims forbidden capability without denying it: {claimed:?}\n{}",
                paragraph_index + 1,
                paragraph.trim().replace('\n', " ")
            );
        }
    }
}
