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
}

export interface AgentRefreshResult {
  agent_name: string
  refreshed: string[]
  deferred: string[]
}

export interface AgentCleanResult {
  cleaned: string
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
export declare function status(root: string, name: string): Promise<AgentCheckResult>
export declare function refresh(root: string, name: string): Promise<AgentRefreshResult>
export declare function land(
  root: string,
  name: string,
  opts?: LandOptions,
): Promise<AgentLandResult>
export declare function clean(root: string, name: string): Promise<AgentCleanResult>
export declare function conflictsKeep(
  root: string,
  path: string,
  keep: KeepChoice,
  filePath?: string,
): Promise<void>
