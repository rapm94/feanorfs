import assert from 'node:assert/strict'
import { test } from 'node:test'

import { inspectTarballs, publishPackages } from '../scripts/publish-packages.mjs'

const tarballDir = process.env.TARBALL_DIR
if (!tarballDir) throw new Error('TARBALL_DIR is required')

function fakeNpm(initial = new Map()) {
  const registry = new Map(initial)
  const published = []
  const packages = inspectTarballs(tarballDir)
  return {
    published,
    registry,
    run(args) {
      if (args[0] === 'view') {
        const name = args[1].slice(0, args[1].lastIndexOf('@'))
        const value = registry.get(name)
        return value
          ? { status: 0, stdout: JSON.stringify(value), stderr: '' }
          : { status: 1, stdout: '', stderr: 'npm error code E404' }
      }
      if (args[0] === 'publish') {
        const pkg = [...packages.values()].find((candidate) => candidate.path === args[1])
        assert.ok(pkg)
        published.push(pkg.metadata.name)
        registry.set(pkg.metadata.name, {
          ...pkg.metadata,
          dist: { integrity: pkg.integrity },
        })
        return { status: 0, stdout: '', stderr: '' }
      }
      throw new Error(`unexpected npm command: ${args.join(' ')}`)
    },
  }
}

test('publishes five platforms before facade and rerun skips exact packages', async () => {
  const npm = fakeNpm()
  await publishPackages({
    tarballDir,
    runNpm: npm.run.bind(npm),
    sleep: async () => {},
    log: () => {},
  })
  assert.deepEqual(npm.published, [
    '@feanorfs/agent-darwin-x64',
    '@feanorfs/agent-darwin-arm64',
    '@feanorfs/agent-linux-x64-gnu',
    '@feanorfs/agent-linux-arm64-gnu',
    '@feanorfs/agent-win32-x64-msvc',
    '@feanorfs/agent',
  ])

  npm.published.length = 0
  await publishPackages({
    tarballDir,
    runNpm: npm.run.bind(npm),
    sleep: async () => {},
    log: () => {},
  })
  assert.deepEqual(npm.published, [])
})

test('rejects registry integrity mismatch before publishing', async () => {
  const packages = inspectTarballs(tarballDir)
  const first = packages.get('@feanorfs/agent-darwin-x64')
  const npm = fakeNpm(
    new Map([
      [first.metadata.name, { ...first.metadata, dist: { integrity: 'sha512-wrong' } }],
    ]),
  )
  await assert.rejects(
    publishPackages({
      tarballDir,
      runNpm: npm.run.bind(npm),
      sleep: async () => {},
      log: () => {},
    }),
    /registry integrity mismatch/,
  )
  assert.deepEqual(npm.published, [])
})

test('rejects registry metadata mismatch before publishing', async () => {
  const packages = inspectTarballs(tarballDir)
  const first = packages.get('@feanorfs/agent-darwin-x64')
  const npm = fakeNpm(
    new Map([
      [
        first.metadata.name,
        {
          ...first.metadata,
          cpu: ['arm64'],
          dist: { integrity: first.integrity },
        },
      ],
    ]),
  )
  await assert.rejects(
    publishPackages({
      tarballDir,
      runNpm: npm.run.bind(npm),
      sleep: async () => {},
      log: () => {},
    }),
    /registry metadata mismatch/,
  )
  assert.deepEqual(npm.published, [])
})

test('dry run reports absent packages without publishing', async () => {
  const npm = fakeNpm()
  await publishPackages({
    tarballDir,
    dryRun: true,
    runNpm: npm.run.bind(npm),
    sleep: async () => {},
    log: () => {},
  })
  assert.deepEqual(npm.published, [])
})
