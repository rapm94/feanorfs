import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  existsSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const repositoryRoot = resolve(packageRoot, '../..')
const npmRoot = join(packageRoot, 'npm')
const artifactsRoot = join(packageRoot, 'artifacts')
const verifyOnly = process.argv.includes('--verify')
const metadataOnly = process.argv.includes('--metadata-only')

const targets = [
  {
    triple: 'x86_64-apple-darwin',
    dir: 'darwin-x64',
    packageName: '@feanorfs/agent-darwin-x64',
    artifact: 'feanorfs-agent-node.darwin-x64.node',
    os: 'darwin',
    cpu: 'x64',
  },
  {
    triple: 'aarch64-apple-darwin',
    dir: 'darwin-arm64',
    packageName: '@feanorfs/agent-darwin-arm64',
    artifact: 'feanorfs-agent-node.darwin-arm64.node',
    os: 'darwin',
    cpu: 'arm64',
  },
  {
    triple: 'x86_64-unknown-linux-gnu',
    dir: 'linux-x64-gnu',
    packageName: '@feanorfs/agent-linux-x64-gnu',
    artifact: 'feanorfs-agent-node.linux-x64-gnu.node',
    os: 'linux',
    cpu: 'x64',
    libc: 'glibc',
  },
  {
    triple: 'aarch64-unknown-linux-gnu',
    dir: 'linux-arm64-gnu',
    packageName: '@feanorfs/agent-linux-arm64-gnu',
    artifact: 'feanorfs-agent-node.linux-arm64-gnu.node',
    os: 'linux',
    cpu: 'arm64',
    libc: 'glibc',
  },
  {
    triple: 'x86_64-pc-windows-msvc',
    dir: 'win32-x64-msvc',
    packageName: '@feanorfs/agent-win32-x64-msvc',
    artifact: 'feanorfs-agent-node.win32-x64-msvc.node',
    os: 'win32',
    cpu: 'x64',
  },
]

class AssemblyError extends Error {}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`)
}

function workspaceVersion() {
  const cargo = readFileSync(join(repositoryRoot, 'Cargo.toml'), 'utf8')
  const section = cargo.match(/\[workspace\.package\]([\s\S]*?)(?:\n\[|$)/)?.[1]
  const version = section?.match(/^version\s*=\s*"([^"]+)"/m)?.[1]
  if (!version) throw new AssemblyError('workspace package version is missing')
  return version
}

function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}

function expectedPlatformManifest(target, facade, version) {
  const manifest = {
    name: target.packageName,
    version,
    os: [target.os],
    cpu: [target.cpu],
    main: target.artifact,
    files: [target.artifact],
    description: facade.description,
    license: facade.license,
    engines: facade.engines,
    publishConfig: { access: 'public' },
    repository: facade.repository,
  }
  if (target.libc) manifest.libc = [target.libc]
  return manifest
}

function syncPackageLock(version, optionalDependencies) {
  const lockPath = join(packageRoot, 'package-lock.json')
  const lock = readJson(lockPath)
  const root = lock.packages?.['']
  if (!root) throw new AssemblyError('package-lock root package is missing')
  const expected = {
    ...lock,
    version,
    packages: {
      ...lock.packages,
      '': {
        ...root,
        version,
        optionalDependencies,
      },
    },
  }
  if (verifyOnly) assertJson(lockPath, expected, 'package lock')
  else writeJson(lockPath, expected)
}

function assertJson(path, expected, label) {
  const actual = JSON.stringify(readJson(path))
  if (actual !== JSON.stringify(expected)) {
    throw new AssemblyError(`${label} metadata drifted; run npm run assemble-packages`)
  }
}

function collectArtifacts() {
  const expectedNames = targets.map((target) => target.artifact).sort()
  const sourceNames = existsSync(artifactsRoot)
    ? readdirSync(artifactsRoot).filter((name) => name.endsWith('.node')).sort()
    : []

  if (sourceNames.length > 0) {
    if (JSON.stringify(sourceNames) !== JSON.stringify(expectedNames)) {
      throw new AssemblyError(
        `artifact set mismatch: expected ${expectedNames.join(', ')}, got ${sourceNames.join(', ')}`,
      )
    }
    const sourceHashes = Object.fromEntries(
      targets.map((target) => [target.artifact, sha256(join(artifactsRoot, target.artifact))]),
    )
    if (!verifyOnly) {
      const napi = join(packageRoot, 'node_modules/@napi-rs/cli/scripts/index.js')
      const result = spawnSync(process.execPath, [napi, 'artifacts'], {
        cwd: packageRoot,
        encoding: 'utf8',
      })
      if (result.status !== 0) {
        throw new AssemblyError(`napi artifacts failed: ${result.stderr || result.stdout}`)
      }
    }
    return sourceHashes
  }

  return Object.fromEntries(
    targets.map((target) => {
      const artifactPath = join(npmRoot, target.dir, target.artifact)
      if (!existsSync(artifactPath)) throw new AssemblyError(`missing artifact: ${target.artifact}`)
      return [target.artifact, sha256(artifactPath)]
    }),
  )
}

function verifyArtifacts(version, expectedHashes) {
  const manifestPath = join(npmRoot, 'artifacts.json')
  const artifacts = targets.map((target) => {
    const path = join(npmRoot, target.dir, target.artifact)
    if (!existsSync(path)) throw new AssemblyError(`missing packaged artifact: ${target.artifact}`)
    const hash = sha256(path)
    if (hash !== expectedHashes[target.artifact]) {
      throw new AssemblyError(`artifact hash changed while collecting: ${target.artifact}`)
    }
    return { target: target.triple, file: target.artifact, sha256: hash }
  })
  const expected = { version, artifacts }
  if (verifyOnly) assertJson(manifestPath, expected, 'artifact manifest')
  else writeJson(manifestPath, expected)
}

function main() {
  const version = workspaceVersion()
  const facadePath = join(packageRoot, 'package.json')
  const facade = readJson(facadePath)
  const optionalDependencies = Object.fromEntries(
    targets.map((target) => [target.packageName, version]),
  )
  const expectedFacade = {
    ...facade,
    version,
    optionalDependencies,
  }
  delete expectedFacade.private

  const allowedDirs = targets.map((target) => target.dir).sort()
  const actualDirs = readdirSync(npmRoot, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort()
  if (JSON.stringify(actualDirs) !== JSON.stringify(allowedDirs)) {
    throw new AssemblyError(`npm package directories drifted: ${actualDirs.join(', ')}`)
  }

  if (verifyOnly) assertJson(facadePath, expectedFacade, 'facade')
  else writeJson(facadePath, expectedFacade)
  syncPackageLock(version, optionalDependencies)

  for (const target of targets) {
    const manifestPath = join(npmRoot, target.dir, 'package.json')
    const expected = expectedPlatformManifest(target, expectedFacade, version)
    if (verifyOnly) assertJson(manifestPath, expected, target.packageName)
    else writeJson(manifestPath, expected)
  }

  if (!metadataOnly) {
    const hashes = collectArtifacts()
    verifyArtifacts(version, hashes)
  }
  const scope = metadataOnly ? 'package manifests' : 'native packages'
  console.log(`Verified ${targets.length} ${scope} at version ${version}`)
}

try {
  main()
} catch (error) {
  if (error instanceof AssemblyError) {
    console.error(error.message)
    process.exitCode = 1
  } else {
    throw error
  }
}
