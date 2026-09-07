// Assemble a host-native, unpublished SDK pair for integration testing.
import fs from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { spawnSync } from 'node:child_process'
import assert from 'node:assert/strict'
const sdk = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const metadata = JSON.parse(await fs.readFile(path.join(sdk, 'package.json'), 'utf8'))
const suffix = process.platform === 'linux' ? `linux-${process.arch}-gnu` : `${process.platform}-${process.arch}${process.platform === 'win32' ? '-msvc' : ''}`
const suffixes = ['darwin-arm64', 'darwin-x64', 'linux-arm64-gnu', 'linux-x64-gnu', 'win32-x64-msvc']
assert.ok(suffixes.includes(suffix))
const destination = path.resolve(process.argv[2] || path.join(sdk, '../../target/local-sdk', metadata.version))
const main = path.join(destination, 'main')
const native = path.join(destination, suffix)
await fs.mkdir(main, { recursive: true }); await fs.mkdir(native, { recursive: true })
for (const file of metadata.files) await fs.copyFile(path.join(sdk, file), path.join(main, file))
await fs.writeFile(path.join(main, 'package.json'), JSON.stringify({ ...metadata, optionalDependencies: Object.fromEntries(suffixes.map(s => [`@eidos.space/graft-${s}`, metadata.version])) }, null, 2))
const binary = `graft-sdk.${suffix}.node`
await fs.copyFile(path.join(sdk, 'native', binary), path.join(native, binary))
await fs.writeFile(path.join(native, 'package.json'), JSON.stringify({
  name: `@eidos.space/graft-${suffix}`, version: metadata.version,
  main: binary, files: [binary], os: [process.platform], cpu: [process.arch],
  ...(process.platform === 'linux' ? { libc: ['glibc'] } : {}), license: metadata.license,
}, null, 2))
for (const dir of [main, native]) {
  const packed = spawnSync('npm', ['pack', '--offline', '--ignore-scripts', '--pack-destination', destination, '--cache', path.join(destination, 'npm-cache')], { cwd: dir, encoding: 'utf8' })
  assert.equal(packed.status, 0, packed.stderr)
  process.stdout.write(path.join(destination, packed.stdout.trim()) + '\n')
}
