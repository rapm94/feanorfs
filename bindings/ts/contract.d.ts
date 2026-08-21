/**
 * Typed contract shapes — mirror docs/agent-api.md and common/src/agent_contract.rs
 */

export interface FileState {
  path: string
  hash: string
  size: number
  mtime: number
  deleted: boolean
  /** Portable executable intent; omitted for non-executable files. */
  mode?: 1
}

export interface SpawnResult {
  agent: string
  files_copied: number
}

export interface AgentListEntry {
  name: string
  state: string
}

export interface AgentListResult {
  agents: AgentListEntry[]
}

export interface AgentListOfflineResult {
  agents: string[]
}

export interface AgentCheckResult {
  agent_name: string
  our_changes: FileState[]
  their_changes: FileState[]
  conflicts: ConcurrentEdit[]
  conflict_risk: string[]
  /** Bounded live continuous-reconciliation projection (present only while active). */
  live?: ContinuousAgentStatus
}

export interface AgentLandResult {
  agent_name: string
  our_changes: FileState[]
  their_changes: FileState[]
  conflicts: ConcurrentEdit[]
  landed: LandedPath[]
  message: string
  readonly snapshot_id?: string
}

export interface AgentRefreshResult {
  agent_name: string
  refreshed: string[]
  deferred: string[]
}

export interface AgentCleanResult {
  cleaned: string
}

export interface LogEntry {
  readonly snapshot_id: string
  readonly parents: readonly string[]
  readonly author: string
  readonly created_at_ms: number
  readonly message: string | null
  readonly changed_paths: readonly string[]
}

export interface LogResult {
  readonly entries: readonly LogEntry[]
}

export interface UndoResult {
  readonly snapshot_id: string
  readonly restored_snapshot_id: string
  readonly changed_paths: readonly string[]
}

export type AgentMessageKind = 'request' | 'status' | 'result' | 'blocked'

export interface AgentMessageInput {
  to: string
  kind: AgentMessageKind
  body: string
  about_snapshot?: string | null
  reply_to?: string | null
  from?: string | null
}

export interface AgentSendResult {
  message_id: string
  about_snapshot: string
}

export interface AgentMessage {
  message_id: string
  from: string
  to: string
  kind: AgentMessageKind
  body: string
  about_snapshot: string
  reply_to: string | null
  created_at_ms: number
}

export interface AgentInboxQuery {
  recipient: string
  after?: string | null
  limit: number
}

export interface AgentInboxResult {
  cursor: string
  cursor_reset: boolean
  messages: AgentMessage[]
}

export interface LandedPath {
  path: string
  action: string
}

export interface ConcurrentEdit {
  path: string
  base?: FileState | null
  ours?: FileState | null
  theirs?: FileState | null
  original_file?: string | null
  local_file?: string | null
  cloud_file?: string | null
  kind?: string | null
  local_available?: boolean
  cloud_available?: boolean
  is_binary?: boolean
  hint?: string | null
  proposed_file?: string | null
  proposal_clean?: boolean | null
}

export interface SpawnOptions {
  noSync?: boolean
  replace?: boolean
}

export interface LandOptions {
  clean?: boolean
  propose?: boolean
}

export type KeepChoice = 0 | 1 | 2 | 3

export declare function listAgents(root: string): Promise<AgentListOfflineResult>
export declare function spawn(
  root: string,
  name: string,
  opts?: SpawnOptions,
): Promise<SpawnResult>
export declare function agentPath(root: string, name: string): Promise<string>
export declare function status(root: string, name: string): Promise<AgentCheckResult>
export declare function refresh(root: string, name: string): Promise<AgentRefreshResult>
export declare function land(
  root: string,
  name: string,
  opts?: LandOptions,
): Promise<AgentLandResult>
export declare function clean(root: string, name: string): Promise<AgentCleanResult>
export declare function log(root: string, limit?: number): Promise<LogResult>
export declare function undo(root: string, snapshotId: string): Promise<UndoResult>
export declare function sendMessage(
  root: string,
  input: AgentMessageInput,
): Promise<AgentSendResult>
export declare function inbox(root: string, query: AgentInboxQuery): Promise<AgentInboxResult>
export declare function conflictsKeep(
  root: string,
  path: string,
  keep: KeepChoice,
  filePath?: string,
): Promise<void>
// --- Randomized integrator assignment (INT-1..INT-15) ---
// Identity and assignment are advisory, not security claims; the hub never
// selects an integrator; FeanorFS never merges file content.

export interface IntegratorCandidate {
  name: string
  capabilities?: string[]
  enabled?: boolean
  available?: boolean
}

export interface IntegratorAssignInput {
  about_snapshot: string
  candidates: IntegratorCandidate[]
  required_capabilities?: string[]
  conflict_authors?: string[]
  excluded?: string[]
  task_summary: string
  ack_timeout_ms?: number | null
}

export type IntegratorAssignmentState =
  | 'created'
  | 'offered'
  | 'accepted'
  | 'completed'
  | 'blocked'
  | 'revoked'
  | 'requires_human'
  | 'cancelled'

export type IntegratorAttemptState =
  | 'offered'
  | 'accepted'
  | 'timed_out'
  | 'superseded'
  | 'revoked'
  | 'blocked'
  | 'completed'

export interface IntegratorAssignResult {
  assignment_id: string
  about_snapshot: string
  selected: string
  fallback_order: string[]
  neutral_integrator: boolean
  roster_fingerprint: string
  attempt: number
  request_message_id: string
  state: IntegratorAssignmentState
  task_summary: string
}

export interface IntegratorAttemptStatus {
  attempt: number
  selected: string
  state: IntegratorAttemptState
  offered_at_ms: number
  request_message_id?: string | null
  terminal_message_id?: string | null
  reason?: string | null
}

export interface VerificationSummary {
  status: 'passed' | 'failed' | 'unknown'
  summary: string
}

export type IntegratorOutcomeState = 'completed' | 'blocked' | 'requires_human' | 'cancelled'

export interface IntegratorDigest {
  assignment_id: string
  integrator: string
  about_snapshot: string
  inspected_snapshot: string
  state: IntegratorOutcomeState
  landed_paths: number
  resolved_conflicts: number
  remaining_conflicts: number
  verification: VerificationSummary
  outcome: string
  risks?: string[]
  decision_required?: string | null
}

export interface IntegratorStatusResult {
  assignment_id: string
  about_snapshot: string
  state: IntegratorAssignmentState
  selected?: string | null
  attempt: number
  neutral_integrator: boolean
  roster_fingerprint: string
  fallback_order: string[]
  task_summary: string
  created_at_ms: number
  updated_at_ms: number
  attempts: IntegratorAttemptStatus[]
  digest?: IntegratorDigest | null
  inbox_cursor?: string | null
}

export interface IntegratorObserveResult {
  assignment_id?: string | null
  state?: IntegratorAssignmentState | null
  messages_processed: number
  cursor?: string | null
  cursor_reset: boolean
  action: string
}

export interface ConflictMaterializeEntry {
  path: string
  kind: 'edit_edit' | 'edit_delete' | 'delete_edit'
  original_available: boolean
  local_available: boolean
  cloud_available: boolean
  is_binary: boolean
  already_materialized: boolean
}

export interface ConflictMaterializeResult {
  about_snapshot: string
  conflict_dir: string
  entries: ConflictMaterializeEntry[]
}

export declare function integratorAssign(root: string, input: IntegratorAssignInput): Promise<IntegratorAssignResult>
export declare function integratorStatus(root: string, assignmentId?: string | null): Promise<IntegratorStatusResult>
export declare function integratorRevoke(root: string, assignmentId: string, reason: string): Promise<IntegratorStatusResult>
export interface IntegratorObserveInput {
  ack_timeout_ms?: number | null
  fallback_on_blocked?: boolean
}

export type ConflictMaterializeInput =
  | { about_snapshot: string; paths: [string, ...string[]]; all?: never }
  | { about_snapshot: string; all: true; paths?: never }

export declare function integratorResume(root: string, options?: IntegratorObserveInput): Promise<IntegratorObserveResult>
export declare function conflictMaterialize(root: string, input: ConflictMaterializeInput): Promise<ConflictMaterializeResult>

// --- Encrypted work-intent protocol (ffwork1, SDK-1 additive) ---
// Proposals and decisions are ordinary ffmsg1 signals carrying ffwork1
// profiles. A sent proposal is never a claim of acceptance: the local
// reducer applies state only after observing the signal.

export type WorkTaskState =
  | 'proposed'
  | 'accepted'
  | 'settled'
  | 'completed'
  | 'blocked'
  | 'yielded'
  | 'rejected'

export type WorkVerificationStatus = 'passed' | 'failed' | 'skipped'

export interface WorkVerification {
  status: WorkVerificationStatus
  summary: string
}

export type WorkOverlapKind =
  | 'exact_path'
  | 'directory_containment'
  | 'glob_match'
  | 'same_concern'

export interface WorkOverlapAcceptance {
  kind: WorkOverlapKind
  path_a?: string
  path_b?: string
  concern?: string
}

export type WorkDecisionKind =
  | { kind: 'accept'; reason?: string | null }
  | { kind: 'reject'; reason: string }
  | { kind: 'narrow'; paths: string[]; concerns: string[]; reason?: string | null }
  | { kind: 'order'; after?: string | null; reason?: string | null }
  | { kind: 'accept_overlap'; overlap: WorkOverlapAcceptance[]; reason?: string | null }

export interface WorkScope {
  paths: string[]
  concerns: string[]
  dependencies: string[]
}

export interface WorkProposeInput {
  task_id: string
  agent?: string | null
  sequence: number
  causal_base?: string | null
  coordinator?: string | null
  paths: string[]
  concerns: string[]
  dependencies: string[]
  capabilities: string[]
  about_snapshot?: string | null
  to?: string | null
}

export interface WorkDecideInput {
  proposal_message_id: string
  kind: WorkDecisionKind
  about_snapshot?: string | null
  to?: string | null
  from?: string | null
}

export interface WorkAmendInput {
  task_id: string
  intent_message_id: string
  sequence: number
  paths?: string[] | null
  concerns?: string[] | null
  dependencies?: string[] | null
  reason?: string | null
  about_snapshot?: string | null
  to?: string | null
  from?: string | null
}

export interface WorkYieldInput {
  task_id: string
  intent_message_id: string
  sequence: number
  reason?: string | null
  about_snapshot?: string | null
  to?: string | null
  from?: string | null
}

export interface WorkSettleInput {
  task_id: string
  intent_message_id: string
  sequence: number
  inspected_snapshot: string
  verification: WorkVerification
  about_snapshot?: string | null
  to?: string | null
  from?: string | null
}

export interface WorkCompleteInput {
  task_id: string
  intent_message_id: string
  sequence: number
  outcome: string
  about_snapshot?: string | null
  to?: string | null
  from?: string | null
}

export interface WorkBlockInput {
  task_id: string
  intent_message_id: string
  sequence: number
  reason: string
  about_snapshot?: string | null
  to?: string | null
  from?: string | null
}

export interface WorkStatusInput {
  coordinator?: string | null
}

export interface WorkSendResult {
  message_id: string
  about_snapshot: string
  task_id: string
  agent: string
  profile: string
  state: WorkTaskState
  scope: WorkScope
  causal_refs: string[]
  overlap: WorkOverlapAcceptance[]
  projection_incomplete: boolean
}

export interface WorkDecisionStatus {
  message_id: string
  coordinator: string
  kind: WorkDecisionKind
  ordered_after?: string | null
}

export interface WorkAmendmentStatus {
  message_id: string
  sequence: number
  reason?: string | null
}

export interface WorkProposalStatus {
  agent: string
  state: WorkTaskState
  sequence: number
  intent_message_id: string
  coordinator?: string | null
  accepted_scope: WorkScope
  decision?: WorkDecisionStatus | null
  accepted_overlap: WorkOverlapAcceptance[]
  amendments: WorkAmendmentStatus[]
  causal_refs: string[]
  inspected_snapshot?: string | null
  verification?: WorkVerification | null
  outcome?: string | null
  reason?: string | null
  source_message_id: string
  updated_at_ms: number
}

export interface WorkTaskStatus {
  task_id: string
  state: WorkTaskState
  proposals: WorkProposalStatus[]
}

export interface WorkStatusResult {
  cursor: string
  cursor_reset: boolean
  projection_incomplete: boolean
  messages_processed: number
  tasks: WorkTaskStatus[]
  evidence_count: number
  dropped_count: number
  updated_at_ms: number
}

export declare function workPropose(root: string, input: WorkProposeInput): Promise<WorkSendResult>
export declare function workDecide(root: string, input: WorkDecideInput): Promise<WorkSendResult>
export declare function workAmend(root: string, input: WorkAmendInput): Promise<WorkSendResult>
export declare function workYield(root: string, input: WorkYieldInput): Promise<WorkSendResult>
export declare function workSettle(root: string, input: WorkSettleInput): Promise<WorkSendResult>
export declare function workComplete(root: string, input: WorkCompleteInput): Promise<WorkSendResult>
export declare function workBlock(root: string, input: WorkBlockInput): Promise<WorkSendResult>
export declare function workStatus(root: string, input?: WorkStatusInput): Promise<WorkStatusResult>

// --- Exact conflict resolution (RES-1..RES-7) ---
// Automatic resolution binds every candidate to the exact current conflict:
// prepare creates one immutable job, submit records a validated resolver
// result (submit NEVER applies), apply publishes with guarded revalidation.
// The hub never merges file content; adapters never reimplement identity
// canonicalization or fingerprinting.

export interface PreventionReason {
  type: 'exhausted' | 'violated'
  detail: string
}

export interface ConflictLegDescriptor {
  present: boolean
  deleted: boolean
  hash: string
  size: number
  mode: number
}

export interface ConflictIdentity {
  schema_version: number
  workspace_id: string
  current_snapshot: string
  about_snapshot: string
  tree_root: string
  path: string
  base: ConflictLegDescriptor
  ours: ConflictLegDescriptor
  theirs: ConflictLegDescriptor
  kind: 'edit_edit' | 'edit_delete' | 'delete_edit'
  task_id?: string | null
  intent_message_ids?: string[]
  assignment_id?: string | null
  attempt?: number | null
  designated_owner?: string | null
  verification_policy?: string | null
}

export interface ArtifactDescriptor {
  role: 'original' | 'local' | 'cloud'
  path: string
}

export interface CandidateDestination {
  path: string
  create_new: boolean
}

export interface VerificationPolicyRef {
  policy_id: string
  command_config_ref: string
  timeout_ms: number
  freshness_required: boolean
}

export interface ResolutionJob {
  schema_version: number
  job_id: string
  task_id: string
  assignment_id: string
  attempt: number
  workspace_id: string
  owner: string
  conflict: ConflictIdentity
  conflict_fingerprint: string
  current_snapshot: string
  about_snapshot: string
  tree_root: string
  accepted_intents?: string[]
  causal_refs?: string[]
  artifacts?: ArtifactDescriptor[]
  candidate_destination: CandidateDestination
  allowed_output_paths?: string[]
  verification: VerificationPolicyRef
  prevention: PreventionReason
  last_resort_reason: string
}

export type ResolutionOutcome =
  | 'candidate_ready'
  | 'no_change_required'
  | 'blocked'
  | 'requires_human'
  | 'failed'
  | 'stale'

export type HumanResolutionReason =
  | 'semantic_ambiguity'
  | 'unavoidable_data_loss'
  | 'missing_or_auth_failed_leg'
  | 'security_compatibility_boundary_change'
  | 'required_verification_unavailable'
  | 'indeterminate_ownership'
  | 'bounded_resolver_exhaustion'
  | 'unsupported_size_safety_bound'
  | 'explicit_product_decision'

export interface CandidateDescriptor {
  path: string
  hash: string
  size: number
  mode: number
  deleted: boolean
}

export interface VerificationSummary {
  status: 'passed' | 'failed' | 'skipped'
  summary: string
}

export interface ResolutionResult {
  schema_version: number
  outcome: ResolutionOutcome
  job_id: string
  assignment_id: string
  attempt: number
  owner: string
  conflict_fingerprint: string
  candidate?: CandidateDescriptor | null
  verification: VerificationSummary
  diagnostics?: string[]
  question?: string | null
  human_reason?: HumanResolutionReason | null
}

export type ResolutionAssignmentState = 'active' | 'revoked' | 'superseded' | 'completed'

export interface ResolutionJobStatus {
  job_id: string
  assignment_id: string
  attempt: number
  owner: string
  conflict_fingerprint: string
  assignment_state: ResolutionAssignmentState
  outcome?: ResolutionOutcome | null
  /** Monotonic per-fingerprint question generation of the escalation this job carries. */
  question_generation: number
  created_at_ms: number
  verified_at_ms?: number | null
}

export interface ResolutionStatusResult {
  schema_version: number
  jobs: ResolutionJobStatus[]
}

export type ResolutionStaleKind =
  | 'head_changed'
  | 'conflict_missing'
  | 'legs_changed'
  | 'identity_mismatch'
  | 'assignment_revoked'
  | 'verification_expired'
  | 'candidate_missing'
  | 'candidate_hash_mismatch'
  | 'candidate_size_mismatch'
  | 'candidate_mode_mismatch'
  | 'candidate_path_mismatch'
  | 'candidate_symlink'

export type ResolutionApplyOutcome =
  | { outcome: 'published'; head: string }
  | { outcome: 'stale'; kind: ResolutionStaleKind; diagnostics: string[] }

export declare function resolutionPrepare(
  root: string,
  path: string,
  prevention: PreventionReason,
): Promise<ResolutionJob>
export declare function resolutionStatus(
  root: string,
  jobId?: string | null,
): Promise<ResolutionStatusResult>
export declare function resolutionSubmit(
  root: string,
  jobId: string,
  result: ResolutionResult,
): Promise<ResolutionResult>
export declare function resolutionApply(
  root: string,
  jobId: string,
): Promise<ResolutionApplyOutcome>

// --- ffres1 protocol operations (SDK-1 additive) ---
// Materialize/put bind every identity field to the engine-owned job; answer
// and publish-answer bind to the live projection so stale answers are
// impossible by construction. Message-id results match the FFI wire shape.

export interface MaterializedResolutionLeg {
  role: 'original' | 'local' | 'cloud'
  path: string
}

export type HumanResolutionOption = 'defer' | 'keep_unresolved' | 'submit_candidate'

export interface HumanResolutionAnswer {
  schema_version: number
  job_id: string
  assignment_id: string
  attempt: number
  conflict_fingerprint: string
  question_generation: number
  chosen_option: HumanResolutionOption
  candidate?: CandidateDescriptor | null
  verification?: VerificationSummary | null
}

export interface MessageIdResult {
  message_id: string
}

export type ProtocolAssignmentState =
  | 'assigned'
  | 'result_received'
  | 'human_answered'
  | 'revoked'

export interface ResolutionProtocolEntryStatus {
  conflict_fingerprint: string
  job_id: string
  assignment_id: string
  attempt: number
  owner: string
  state: ProtocolAssignmentState
  question_generation: number
  outcome?: ResolutionOutcome | null
  question?: string | null
}

export interface ResolutionProtocolStatus {
  schema_version: number
  cursor?: string | null
  projection_incomplete: boolean
  entries: ResolutionProtocolEntryStatus[]
}

export declare function resolutionMaterialize(
  root: string,
  jobId: string,
): Promise<MaterializedResolutionLeg[]>
export declare function resolutionPut(
  root: string,
  jobId: string,
  base64: string,
): Promise<CandidateDescriptor>
export declare function resolutionAnswer(
  root: string,
  answer: HumanResolutionAnswer,
): Promise<HumanResolutionAnswer>
export declare function resolutionDefer(root: string, jobId: string): Promise<null>
export declare function resolutionProtocolStatus(
  root: string,
  rebuild?: boolean,
): Promise<ResolutionProtocolStatus>
export declare function resolutionAssign(root: string, jobId: string): Promise<MessageIdResult>
export declare function resolutionReply(root: string, jobId: string): Promise<MessageIdResult>
export declare function resolutionRevoke(
  root: string,
  jobId: string,
  superseded?: boolean,
): Promise<MessageIdResult>
export declare function resolutionPublishAnswer(
  root: string,
  answer: HumanResolutionAnswer,
): Promise<MessageIdResult>

export interface ContinuousAttention {
  reason: string
  detail: string
}

export interface ContinuousAgentStatus {
  schema_version: number
  agent: string
  active: boolean
  phase: 'starting' | 'idle' | 'local_dirty' | 'reconciling_local' | 'refreshing_remote' | 'offline' | 'needs_attention' | 'stopping'
  observed_head?: string
  observed_tree?: string
  settled_snapshot?: string
  pending_local: boolean
  deferred_count: number
  attention?: ContinuousAttention | null
  owner_pid?: number
  owner_start_id?: string
  updated_at_ms: number
}
