import assert from 'node:assert/strict'
import { cpSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const target = {
  'darwin-x64': ['darwin-x64', 'feanorfs-agent-node.darwin-x64.node'],
  'darwin-arm64': ['darwin-arm64', 'feanorfs-agent-node.darwin-arm64.node'],
  'linux-x64': ['linux-x64-gnu', 'feanorfs-agent-node.linux-x64-gnu.node'],
  'linux-arm64': ['linux-arm64-gnu', 'feanorfs-agent-node.linux-arm64-gnu.node'],
  'win32-x64': ['win32-x64-msvc', 'feanorfs-agent-node.win32-x64-msvc.node'],
}[`${process.platform}-${process.arch}`]

if (!target) {
  console.log(`loader version test skipped on ${process.platform}-${process.arch}`)
  process.exit(0)
}

const [suffix, artifact] = target
const packageName = `@feanorfs/agent-${suffix}`
const temp = mkdtempSync(path.join(os.tmpdir(), 'feanorfs-node-loader-'))
try {
  cpSync(path.join(packageRoot, 'index.js'), path.join(temp, 'index.js'))
  cpSync(path.join(packageRoot, 'package.json'), path.join(temp, 'package.json'))
  const nativePackage = path.join(temp, 'node_modules', ...packageName.split('/'))
  mkdirSync(nativePackage, { recursive: true })
  cpSync(path.join(packageRoot, artifact), path.join(nativePackage, artifact))
  const manifest = {
    name: packageName,
    version: JSON.parse(readFileSync(path.join(packageRoot, 'package.json'), 'utf8')).version,
    main: artifact,
  }
  const run = () =>
    spawnSync(process.execPath, ['-e', `require(${JSON.stringify(path.join(temp, 'index.js'))})`], {
      encoding: 'utf8',
      env: { ...process.env, NAPI_RS_ENFORCE_VERSION_CHECK: '1' },
    })

  writeFileSync(path.join(nativePackage, 'package.json'), `${JSON.stringify(manifest)}
`)
  const matching = run()
  assert.equal(matching.status, 0, matching.stderr || matching.stdout)

  manifest.version = '0.0.0'
  writeFileSync(path.join(nativePackage, 'package.json'), `${JSON.stringify(manifest)}
`)
  const mismatched = run()
  assert.notEqual(mismatched.status, 0)
  assert.match(`${mismatched.stderr}${mismatched.stdout}`, /version mismatch/)
  console.log('Native loader strict version checks OK')
} finally {
  rmSync(temp, { recursive: true, force: true })
}
