/**
 * N2: loader errors (unsupported platform / arch, browser-like import,
 * missing native binding) are loader-level failures, tested independently
 * from contract errors (bounds, JSON schemas, engine failures).
 *
 * The generated `index.js` loader is Node-only: it must fail with a clean
 * loader error before any contract/engine logic runs whenever the host is
 * unsupported or the binary is absent.
 */
import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, rmSync, cpSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { spawnSync } from 'node:child_process'
import vm from 'node:vm'
import { fileURLToPath } from 'node:url'

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const loaderPath = path.join(packageRoot, 'index.js')

function runLoader(code) {
  return spawnSync(process.execPath, ['-e', code], { encoding: 'utf8' })
}

// `process.platform`/`process.arch` are configurable; the prelude redefines
// them in the same process before the loader runs.
const requireLoader = `require(${JSON.stringify(loaderPath)})`

testUnsupportedPlatform()
testUnsupportedArchitecture()
testBrowserImportIsALoaderErrorNotAContractError()
testMissingNativeBindingFailsAsLoaderError()

function testUnsupportedPlatform() {
  const result = runLoader(
    `Object.defineProperty(process, 'platform', { value: 'haiku' }); ${requireLoader}`,
  )
  assert.notEqual(result.status, 0)
  assert.match(`${result.stderr}${result.stdout}`, /Unsupported OS: haiku/)
}

function testUnsupportedArchitecture() {
  const result = runLoader(
    `Object.defineProperty(process, 'arch', { value: 'mips' }); ${requireLoader}`,
  )
  assert.notEqual(result.status, 0)
  assert.match(
    `${result.stderr}${result.stdout}`,
    /Unsupported architecture on (macOS|Linux|Windows|FreeBSD|Android): mips/,
  )
}

function testBrowserImportIsALoaderErrorNotAContractError() {
  const source = readFileSync(loaderPath, 'utf8')
  let threw = false
  let message = ''
  try {
    // A browser context has no Node `require`/`process` globals; the loader
    // must fail as a loader-level ReferenceError, never reaching workspace
    // or contract logic (which would imply a bundled polyfill).
    vm.runInNewContext(source, {}, { timeout: 5000 })
  } catch (error) {
    threw = true
    message = error?.message ? String(error.message) : String(error)
  }
  assert.ok(threw, 'loader must throw in a browser-like context')
  assert.match(message, /require is not defined|process is not defined/)
}

function testMissingNativeBindingFailsAsLoaderError() {
  const temp = mkdtempSync(path.join(os.tmpdir(), 'feanorfs-node-loader-error-'))
  try {
    // index.js alone, no .node artifact and no platform package: the loader
    // must fail with the canonical missing-binding error, not a contract
    // error (invalid JSON / bounds / engine failure).
    cpSync(loaderPath, path.join(temp, 'index.js'))
    const result = spawnSync(
      process.execPath,
      ['-e', `require(${JSON.stringify(path.join(temp, 'index.js'))})`],
      {
        encoding: 'utf8',
        env: { ...process.env, NAPI_RS_NATIVE_LIBRARY_PATH: '' },
      },
    )
    assert.notEqual(result.status, 0)
    assert.match(`${result.stderr}${result.stdout}`, /Cannot find native binding/)
  } finally {
    rmSync(temp, { recursive: true, force: true })
  }
}

console.log('Loader error isolation checks OK')
