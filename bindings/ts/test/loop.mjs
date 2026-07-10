/**
 * SDK-5: typed async loop via @feanorfs/agent (workspace setup uses CLI).
 */
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const agentModule = process.env.FEANORFS_AGENT_IMPORT ?? '../api.mjs';
const { spawn, land, clean, refresh, conflictsKeep } = await import(agentModule);

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

  const agentDir = path.join(ws, '.feanorfs/agents/worker');
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

  console.log('SDK-5 Node loop OK');
} finally {
  fs.rmSync(demo, { recursive: true, force: true });
}
