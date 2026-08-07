/**
 * Typed contract shapes — mirror docs/agent-api.md and common/src/agent_contract.rs
 */

export interface FileState {
  path: string
  hash: string
  size: number
  mtime: number
  deleted: boolean
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
  | 'active'
  | 'completed'
  | 'blocked'
  | 'revoked'
  | 'requires_human'
  | 'cancelled'

export type IntegratorAttemptState =
  | 'offered'
  | 'accepted'
  | 'active'
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
export declare function integratorResume(root: string, options?: { ack_timeout_ms?: number; fallback_on_blocked?: boolean }): Promise<IntegratorObserveResult>
export declare function conflictMaterialize(root: string, input: { about_snapshot: string; paths?: string[] }): Promise<ConflictMaterializeResult>
