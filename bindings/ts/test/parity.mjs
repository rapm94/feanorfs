/**
 * N2: the JS facade and TypeScript declarations must stay in exact parity
 * with the native napi exports. The canonical operation matrix lives in
 * client/tests/operation_matrix.rs (marker checks); this test validates the
 * same contract at runtime: every operation is callable on the native
 * module, exposed on the api.mjs facade, and declared in contract.d.ts.
 * A missing facade or declaration method fails CI.
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { createRequire } from 'node:module'

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const require = createRequire(import.meta.url)

const native = require(path.join(packageRoot, 'index.js'))
const facade = await import(path.join(packageRoot, 'api.mjs'))
const declarations = readFileSync(path.join(packageRoot, 'contract.d.ts'), 'utf8')

// Every baseline operation: native export name, api.mjs facade name, and the
// `declare function` name in contract.d.ts. Keep in sync with
// client/tests/operation_matrix.rs.
const OPERATIONS = [
  ['agentList', 'listAgents', 'listAgents'],
  ['agentSpawn', 'spawn', 'spawn'],
  ['agentPath', 'agentPath', 'agentPath'],
  ['agentStatus', 'status', 'status'],
  ['agentRefresh', 'refresh', 'refresh'],
  ['agentLand', 'land', 'land'],
  ['agentClean', 'clean', 'clean'],
  ['historyLog', 'log', 'log'],
  ['undo', 'undo', 'undo'],
  ['agentSend', 'sendMessage', 'sendMessage'],
  ['agentInbox', 'inbox', 'inbox'],
  ['conflictsKeep', 'conflictsKeep', 'conflictsKeep'],
  ['integratorAssign', 'integratorAssign', 'integratorAssign'],
  ['integratorStatus', 'integratorStatus', 'integratorStatus'],
  ['integratorRevoke', 'integratorRevoke', 'integratorRevoke'],
  ['integratorResume', 'integratorResume', 'integratorResume'],
  ['conflictMaterialize', 'conflictMaterialize', 'conflictMaterialize'],
  ['workPropose', 'workPropose', 'workPropose'],
  ['workDecide', 'workDecide', 'workDecide'],
  ['workAmend', 'workAmend', 'workAmend'],
  ['workYield', 'workYield', 'workYield'],
  ['workSettle', 'workSettle', 'workSettle'],
  ['workComplete', 'workComplete', 'workComplete'],
  ['workBlock', 'workBlock', 'workBlock'],
  ['workStatus', 'workStatus', 'workStatus'],
  ['resolutionPrepare', 'resolutionPrepare', 'resolutionPrepare'],
  ['resolutionStatus', 'resolutionStatus', 'resolutionStatus'],
  ['resolutionSubmit', 'resolutionSubmit', 'resolutionSubmit'],
  ['resolutionApply', 'resolutionApply', 'resolutionApply'],
  ['resolutionMaterialize', 'resolutionMaterialize', 'resolutionMaterialize'],
  ['resolutionPut', 'resolutionPut', 'resolutionPut'],
  ['resolutionAnswer', 'resolutionAnswer', 'resolutionAnswer'],
  ['resolutionDefer', 'resolutionDefer', 'resolutionDefer'],
  ['resolutionProtocolStatus', 'resolutionProtocolStatus', 'resolutionProtocolStatus'],
  ['resolutionAssign', 'resolutionAssign', 'resolutionAssign'],
  ['resolutionReply', 'resolutionReply', 'resolutionReply'],
  ['resolutionRevoke', 'resolutionRevoke', 'resolutionRevoke'],
  ['resolutionPublishAnswer', 'resolutionPublishAnswer', 'resolutionPublishAnswer'],
]

for (const [nativeName, facadeName, declareName] of OPERATIONS) {
  assert.equal(
    typeof native[nativeName],
    'function',
    `native export ${nativeName} is missing from the generated loader`,
  )
  assert.equal(
    typeof facade[facadeName],
    'function',
    `facade method ${facadeName} is missing from api.mjs`,
  )
  assert.ok(
    declarations.includes(`declare function ${declareName}(`),
    `declaration for ${declareName} is missing from contract.d.ts`,
  )
}

// The loader must also surface every native export directly (the generated
// `module.exports.<name>` tail), so `require('@feanorfs/agent/native')`
// matches api.mjs.
for (const [nativeName] of OPERATIONS) {
  assert.equal(typeof native[nativeName], 'function', `native ${nativeName}`)
}

console.log(`Facade/declaration parity OK (${OPERATIONS.length} operations)`)
