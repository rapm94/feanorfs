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

export async function conflictsKeep(root, path, keep, filePath) {
  await native.conflictsKeep(root, path, keep, filePath ?? null)
}

export { native }
