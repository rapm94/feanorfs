/**
 * SDK-5: typed async loop via @feanorfs/agent (workspace setup uses CLI).
 */
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const agentModule = process.env.FEANORFS_AGENT_IMPORT ?? '../api.mjs';
const {
  spawn,
  agentPath,
  land,
  clean,
  refresh,
  conflictsKeep,
  sendMessage,
  inbox,
  integratorAssign,
  integratorStatus,
  integratorRevoke,
  integratorResume,
  conflictMaterialize,
} = await import(agentModule);

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..');
const feanorfs =
  process.env.FEANORFS_BIN ?? path.join(repoRoot, 'target/debug/feanorfs');

function runFeanorfs(cwd, ...args) {
  execFileSync(feanorfs, args, { cwd, stdio: 'inherit' });
}

const demo = fs.mkdtempSync(path.join(os.tmpdir(), 'feanorfs-node-'));
const ws = path.join(demo, 'workspace');
fs.mkdirSync(ws, { recursive: true });

try {
  runFeanorfs(ws, 'start', '--local', '--workspace', 'node-demo', '--no-watch');
  fs.writeFileSync(path.join(ws, 'seed.txt'), 'seed\n');
  runFeanorfs(ws, 'sync', '--no-watch');

  const spawnResult = await spawn(ws, 'worker', {});
  if (spawnResult.files_copied !== 1) {
    throw new Error(`unexpected spawn: ${JSON.stringify(spawnResult)}`);
  }

  const agentDir = await agentPath(ws, 'worker');
  if (agentDir.startsWith(ws) || fs.existsSync(path.join(ws, '.feanorfs'))) {
    throw new Error(`agent state leaked into project: ${agentDir}`);
  }
  fs.writeFileSync(path.join(agentDir, 'task.txt'), 'node edit\n');

  const landResult = await land(ws, 'worker', {});
  if (!landResult.landed?.length && !landResult.message) {
    throw new Error(`land failed: ${JSON.stringify(landResult)}`);
  }

  const refreshResult = await refresh(ws, 'worker');
  if (!refreshResult.agent_name) {
    throw new Error(`refresh failed: ${JSON.stringify(refreshResult)}`);
  }

  let conflictErr = false;
  try {
    await conflictsKeep(ws, 'nonexistent', 999);
  } catch (_) {
    conflictErr = true;
  }
  if (!conflictErr) throw new Error('conflictsKeep(999) should have thrown');

  const cleanResult = await clean(ws, 'worker');
  if (cleanResult.cleaned !== 'worker') {
    throw new Error(`clean failed: ${JSON.stringify(cleanResult)}`);
  }

  const sent = await sendMessage(ws, {
    to: 'mac-test',
    kind: 'request',
    body: 'Run iOS simulator tests',
    from: 'node-agent',
  });
  if (!sent.message_id || !sent.about_snapshot) {
    throw new Error(`sendMessage failed: ${JSON.stringify(sent)}`);
  }
  const inboxResult = await inbox(ws, { recipient: 'mac-test', limit: 50 });
  const delivered = inboxResult.messages.find((m) => m.message_id === sent.message_id);
  if (!delivered || delivered.from !== 'node-agent' || delivered.body !== 'Run iOS simulator tests') {
    throw new Error(`inbox missed signal: ${JSON.stringify(inboxResult)}`);
  }
  const delta = await inbox(ws, {
    recipient: 'mac-test',
    after: sent.about_snapshot,
    limit: 50,
  });
  if (!delta.messages.some((m) => m.message_id === sent.message_id)) {
    throw new Error(`inbox cursor delta missed signal: ${JSON.stringify(delta)}`);
  }

  // Randomized integrator assignment smoke (SDK-1 additive).
  const { log } = await import(agentModule);
  const history = await log(ws, 5);
  const head = history.entries[0].snapshot_id;
  const assigned = await integratorAssign(ws, {
    about_snapshot: head,
    candidates: [
      { name: 'agent-a', capabilities: ['rust'] },
      { name: 'agent-b', capabilities: ['rust', 'ios'] },
    ],
    required_capabilities: ['rust'],
    task_summary: 'Integrate parser implementation and tests',
    ack_timeout_ms: 300000,
  });
  if (!assigned.assignment_id || !assigned.selected || assigned.state !== 'offered') {
    throw new Error(`integratorAssign failed: ${JSON.stringify(assigned)}`);
  }
  const status = await integratorStatus(ws, assigned.assignment_id);
  if (status.state !== 'offered' || status.attempt !== 0) {
    throw new Error(`integratorStatus failed: ${JSON.stringify(status)}`);
  }
  const resumed = await integratorResume(ws, { ack_timeout_ms: 300000 });
  if (resumed.action !== 'none' && resumed.action !== 'offered_next') {
    throw new Error(`integratorResume failed: ${JSON.stringify(resumed)}`);
  }
  const revoked = await integratorRevoke(ws, assigned.assignment_id, 'node smoke test');
  if (revoked.state !== 'cancelled' && revoked.state !== 'offered') {
    throw new Error(`integratorRevoke failed: ${JSON.stringify(revoked)}`);
  }
  const materialized = await conflictMaterialize(ws, { about_snapshot: head, paths: [] });
  if (!Array.isArray(materialized.entries)) {
    throw new Error(`conflictMaterialize failed: ${JSON.stringify(materialized)}`);
  }

  console.log('SDK-5 Node loop OK');
} finally {
  fs.rmSync(demo, { recursive: true, force: true });
}
