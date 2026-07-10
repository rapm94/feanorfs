import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { readdirSync, readFileSync } from 'node:fs'
import { basename, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const platformPackages = [
  '@feanorfs/agent-darwin-x64',
  '@feanorfs/agent-darwin-arm64',
  '@feanorfs/agent-linux-x64-gnu',
  '@feanorfs/agent-linux-arm64-gnu',
  '@feanorfs/agent-win32-x64-msvc',
]
const facadePackage = '@feanorfs/agent'

class PublishError extends Error {}

function command(program, args) {
  return spawnSync(program, args, { encoding: 'utf8' })
}

function tarballPackageJson(path) {
  const result = command('tar', ['-xOf', path, 'package/package.json'])
  if (result.status !== 0) {
    throw new PublishError(`cannot read ${basename(path)}: ${result.stderr}`)
  }
  return JSON.parse(result.stdout)
}

function integrity(path) {
  const digest = createHash('sha512').update(readFileSync(path)).digest('base64')
  return `sha512-${digest}`
}

function comparableMetadata(value) {
  const optionalDependencies = value.optionalDependencies
    ? Object.fromEntries(
        Object.entries(value.optionalDependencies).sort(([left], [right]) =>
          left.localeCompare(right),
        ),
      )
    : null
  return {
    name: value.name,
    version: value.version,
    os: value.os ?? null,
    cpu: value.cpu ?? null,
    libc: value.libc ?? null,
    optionalDependencies,
  }
}

function inspectTarballs(tarballDir) {
  const packages = new Map()
  const names = readdirSync(tarballDir).filter((name) => name.endsWith('.tgz'))
  for (const name of names) {
    const path = resolve(tarballDir, name)
    const metadata = tarballPackageJson(path)
    if (packages.has(metadata.name)) {
      throw new PublishError(`duplicate tarball for ${metadata.name}`)
    }
    packages.set(metadata.name, { path, metadata, integrity: integrity(path) })
  }
  const expected = [...platformPackages, facadePackage]
  const actual = [...packages.keys()].sort()
  if (JSON.stringify(actual) !== JSON.stringify(expected.sort())) {
    throw new PublishError(`tarball set mismatch: ${actual.join(', ')}`)
  }
  return packages
}

function registryPackage(pkg, runNpm) {
  const spec = `${pkg.metadata.name}@${pkg.metadata.version}`
  const result = runNpm(['view', spec, '--json'])
  if (result.status === 0) return JSON.parse(result.stdout)
  if (`${result.stderr}\n${result.stdout}`.includes('E404')) return null
  throw new PublishError(`npm view failed for ${spec}: ${result.stderr || result.stdout}`)
}

function verifyRegistryPackage(pkg, registry) {
  if (registry.dist?.integrity !== pkg.integrity) {
    throw new PublishError(`registry integrity mismatch for ${pkg.metadata.name}`)
  }
  if (
    JSON.stringify(comparableMetadata(registry)) !==
    JSON.stringify(comparableMetadata(pkg.metadata))
  ) {
    throw new PublishError(`registry metadata mismatch for ${pkg.metadata.name}`)
  }
}

async function waitForRegistry(pkg, runNpm, sleep) {
  for (let attempt = 0; attempt < 12; attempt += 1) {
    const registry = registryPackage(pkg, runNpm)
    if (registry) {
      verifyRegistryPackage(pkg, registry)
      return
    }
    await sleep(5000)
  }
  throw new PublishError(`registry did not expose ${pkg.metadata.name} after publish`)
}

async function ensurePublished(pkg, options) {
  const registry = registryPackage(pkg, options.runNpm)
  if (registry) {
    verifyRegistryPackage(pkg, registry)
    options.log(`verified existing ${pkg.metadata.name}@${pkg.metadata.version}`)
    return
  }
  if (options.dryRun) {
    options.log(`would publish ${pkg.metadata.name}@${pkg.metadata.version}`)
    return
  }
  const result = options.runNpm([
    'publish',
    pkg.path,
    '--access',
    'public',
    '--provenance',
  ])
  if (result.status !== 0) {
    throw new PublishError(`npm publish failed for ${pkg.metadata.name}: ${result.stderr}`)
  }
  await waitForRegistry(pkg, options.runNpm, options.sleep)
  options.log(`published ${pkg.metadata.name}@${pkg.metadata.version}`)
}

export async function publishPackages({
  tarballDir,
  dryRun = false,
  runNpm = (args) => command('npm', args),
  sleep = (milliseconds) => new Promise((resolveSleep) => setTimeout(resolveSleep, milliseconds)),
  log = console.log,
}) {
  const packages = inspectTarballs(tarballDir)
  const options = { dryRun, runNpm, sleep, log }
  for (const name of platformPackages) await ensurePublished(packages.get(name), options)
  if (!dryRun) {
    for (const name of platformPackages) {
      const pkg = packages.get(name)
      const registry = registryPackage(pkg, runNpm)
      if (!registry) throw new PublishError(`platform package disappeared: ${name}`)
      verifyRegistryPackage(pkg, registry)
    }
  }
  await ensurePublished(packages.get(facadePackage), options)
}

function argumentValue(name) {
  const index = process.argv.indexOf(name)
  return index === -1 ? null : process.argv[index + 1]
}

async function main() {
  const tarballDir = argumentValue('--tarball-dir')
  if (!tarballDir) throw new PublishError('--tarball-dir is required')
  await publishPackages({ tarballDir, dryRun: process.argv.includes('--dry-run') })
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    await main()
  } catch (error) {
    if (error instanceof PublishError) {
      console.error(error.message)
      process.exitCode = 1
    } else {
      throw error
    }
  }
}

export { inspectTarballs }
