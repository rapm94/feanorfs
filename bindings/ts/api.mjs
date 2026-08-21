/**
 * Typed async API over the napi native module.
 * Each call opens the workspace fresh (same as FFI).
 */
import { createRequire } from 'node:module'

const require = createRequire(import.meta.url)
const native = require('./index.js')

/** @typedef {import('./contract.d.ts').SpawnOptions} SpawnOptions */
/** @typedef {import('./contract.d.ts').LandOptions} LandOptions */
/** @typedef {import('./contract.d.ts').KeepChoice} KeepChoice */

export async function listAgents(root) {
  return JSON.parse(await native.agentList(root))
}

export async function spawn(root, name, opts = {}) {
  return JSON.parse(await native.agentSpawn(root, name, opts))
}

export async function agentPath(root, name) {
  return native.agentPath(root, name)
}

export async function status(root, name) {
  return JSON.parse(await native.agentStatus(root, name))
}

export async function refresh(root, name) {
  return JSON.parse(await native.agentRefresh(root, name))
}

export async function land(root, name, opts = {}) {
  return JSON.parse(await native.agentLand(root, name, opts))
}

export async function clean(root, name) {
  return JSON.parse(await native.agentClean(root, name))
}

export async function log(root, limit = 20) {
  return JSON.parse(await native.historyLog(root, limit))
}

export async function undo(root, snapshotId) {
  return JSON.parse(await native.undo(root, snapshotId))
}

export async function sendMessage(root, input) {
  return JSON.parse(await native.agentSend(root, JSON.stringify(input)))
}

export async function inbox(root, query) {
  return JSON.parse(await native.agentInbox(root, JSON.stringify(query)))
}

export async function conflictsKeep(root, path, keep, filePath) {
  await native.conflictsKeep(root, path, keep, filePath ?? null)
}

/** Assign one batch to a randomly ranked integrator. */
export async function integratorAssign(root, input) {
  return JSON.parse(await native.integratorAssign(root, JSON.stringify(input)))
}

/** Read the active integrator assignment (or one by id). */
export async function integratorStatus(root, assignmentId) {
  return JSON.parse(await native.integratorStatus(root, assignmentId ?? null))
}

/** Explicitly revoke the active integrator assignment. */
export async function integratorRevoke(root, assignmentId, reason) {
  return JSON.parse(await native.integratorRevoke(root, assignmentId, reason))
}

/** Resume dispatcher observation after a restart. */
export async function integratorResume(root, options = {}) {
  return JSON.parse(await native.integratorResume(root, JSON.stringify(options)))
}

/** Materialize the encrypted conflict triple for a snapshot (read-only). */
export async function conflictMaterialize(root, input) {
  return JSON.parse(await native.conflictMaterialize(root, JSON.stringify(input)))
}

/** Propose one encrypted work intent. */
export async function workPropose(root, input) {
  return JSON.parse(await native.workPropose(root, JSON.stringify(input)))
}

/** Send one coordinator decision for an exact proposal. */
export async function workDecide(root, input) {
  return JSON.parse(await native.workDecide(root, JSON.stringify(input)))
}

/** Amend an accepted intent's scope. */
export async function workAmend(root, input) {
  return JSON.parse(await native.workAmend(root, JSON.stringify(input)))
}

/** Send an explicit yield relinquishing accepted overlap. */
export async function workYield(root, input) {
  return JSON.parse(await native.workYield(root, JSON.stringify(input)))
}

/** Send a settled profile with verification evidence. */
export async function workSettle(root, input) {
  return JSON.parse(await native.workSettle(root, JSON.stringify(input)))
}

/** Send a terminal completion. */
export async function workComplete(root, input) {
  return JSON.parse(await native.workComplete(root, JSON.stringify(input)))
}

/** Send a terminal blocker. */
export async function workBlock(root, input) {
  return JSON.parse(await native.workBlock(root, JSON.stringify(input)))
}

/** Observe signals through the reducer and report the bounded projection. */
export async function workStatus(root, input = {}) {
  return JSON.parse(await native.workStatus(root, JSON.stringify(input)))
}

/**
 * Prepare one automatic resolution job for the exact current conflict.
 * Requires a typed prevention reason: `{ type: 'exhausted' | 'violated',
 * detail: string }`. Read-only: never mutates the worktree, conflict
 * registry, artifacts, or head.
 */
export async function resolutionPrepare(root, path, prevention) {
  return JSON.parse(await native.resolutionPrepare(root, path, JSON.stringify(prevention)))
}

/** Read the bounded resolution status projection (ids/state/counts only). */
export async function resolutionStatus(root, jobId) {
  return JSON.parse(await native.resolutionStatus(root, jobId ?? null))
}

/**
 * Submit one resolution result. Submission NEVER applies: it validates and
 * records the result without mutating anything. Apply is a separate explicit
 * operation.
 */
export async function resolutionSubmit(root, jobId, result) {
  return JSON.parse(await native.resolutionSubmit(root, jobId, JSON.stringify(result)))
}

/**
 * Apply a submitted resolution result with guarded publication: revalidates
 * every identity field and the candidate descriptor immediately before a
 * single CAS.
 */
export async function resolutionApply(root, jobId) {
  return JSON.parse(await native.resolutionApply(root, jobId))
}

/**
 * Materialize the authenticated base/ours/theirs legs of one resolution job
 * into the engine-owned job directory. Read-only.
 */
export async function resolutionMaterialize(root, jobId) {
  return JSON.parse(await native.resolutionMaterialize(root, jobId))
}

/**
 * Write the immutable engine-owned candidate file for one job from bounded
 * base64 bytes and return its plaintext descriptor.
 */
export async function resolutionPut(root, jobId, base64) {
  return JSON.parse(await native.resolutionPut(root, jobId, base64))
}

/**
 * Record one typed human answer bound to one exact escalation. Never
 * publishes; use resolutionPublishAnswer for the ffres1 profile.
 */
export async function resolutionAnswer(root, answer) {
  return JSON.parse(await native.resolutionAnswer(root, JSON.stringify(answer)))
}

/** Record the terminal Deferred state for one assignment without publication. */
export async function resolutionDefer(root, jobId) {
  return JSON.parse(await native.resolutionDefer(root, jobId))
}

/**
 * Observe the encrypted signal stream through the ffres1 reducer and report
 * the bounded metadata-only projection.
 */
export async function resolutionProtocolStatus(root, rebuild = false) {
  return JSON.parse(await native.resolutionProtocolStatus(root, rebuild))
}

/** Publish the ffres1 assignment profile for one locally prepared job. */
export async function resolutionAssign(root, jobId) {
  return JSON.parse(await native.resolutionAssign(root, jobId))
}

/** Publish the ffres1 result profile for one locally submitted job. */
export async function resolutionReply(root, jobId) {
  return JSON.parse(await native.resolutionReply(root, jobId))
}

/** Publish the ffres1 revoke/supersede profile for one local job. */
export async function resolutionRevoke(root, jobId, superseded = false) {
  return JSON.parse(await native.resolutionRevoke(root, jobId, superseded))
}

/** Publish one typed human answer as an ffres1 profile. */
export async function resolutionPublishAnswer(root, answer) {
  return JSON.parse(await native.resolutionPublishAnswer(root, JSON.stringify(answer)))
}

export { native }
