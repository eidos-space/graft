import assert from "node:assert/strict"
import fs from "node:fs/promises"
import path from "node:path"
import { fileURLToPath } from "node:url"

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const rootPackage = JSON.parse(
  await fs.readFile(path.join(packageRoot, "package.json"), "utf8")
)

const targets = [
  {
    suffix: "darwin-arm64",
    triple: "aarch64-apple-darwin",
    os: ["darwin"],
    cpu: ["arm64"],
  },
  {
    suffix: "darwin-x64",
    triple: "x86_64-apple-darwin",
    os: ["darwin"],
    cpu: ["x64"],
  },
  {
    suffix: "linux-arm64-gnu",
    triple: "aarch64-unknown-linux-gnu",
    os: ["linux"],
    cpu: ["arm64"],
    libc: ["glibc"],
  },
  {
    suffix: "linux-x64-gnu",
    triple: "x86_64-unknown-linux-gnu",
    os: ["linux"],
    cpu: ["x64"],
    libc: ["glibc"],
  },
  {
    suffix: "win32-x64-msvc",
    triple: "x86_64-pc-windows-msvc",
    os: ["win32"],
    cpu: ["x64"],
  },
]

assert.equal(rootPackage.name, "@eidos.space/graft")
assert.match(rootPackage.version, /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/)
assert.deepEqual(rootPackage.napi.targets.sort(), targets.map(({ triple }) => triple).sort())

const packageDirectory = path.join(packageRoot, "npm")
const actualDirectories = (await fs.readdir(packageDirectory)).sort()
assert.deepEqual(actualDirectories, targets.map(({ suffix }) => suffix).sort())

for (const target of targets) {
  const targetRoot = path.join(packageDirectory, target.suffix)
  const metadata = JSON.parse(
    await fs.readFile(path.join(targetRoot, "package.json"), "utf8")
  )
  const expectedName = `@eidos.space/graft-${target.suffix}`
  const expectedBinary = `graft-sdk.${target.suffix}.node`
  const files = await fs.readdir(targetRoot)

  assert.equal(metadata.name, expectedName)
  assert.equal(metadata.version, rootPackage.version)
  assert.deepEqual(metadata.os, target.os)
  assert.deepEqual(metadata.cpu, target.cpu)
  if (target.libc) assert.deepEqual(metadata.libc, target.libc)
  assert.equal(metadata.main, expectedBinary)
  assert.deepEqual(
    files.filter((file) => file.endsWith(".node")),
    [expectedBinary]
  )
  assert.equal(
    rootPackage.optionalDependencies[expectedName],
    rootPackage.version
  )
  const stats = await fs.stat(path.join(targetRoot, expectedBinary))
  assert.ok(stats.size > 1_000_000, `${expectedBinary} is unexpectedly small`)
}

console.log(
  `Verified ${rootPackage.name}@${rootPackage.version} and ${targets.length} native packages`
)
