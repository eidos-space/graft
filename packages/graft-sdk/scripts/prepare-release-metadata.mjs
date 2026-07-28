import fs from "node:fs/promises"
import path from "node:path"
import { fileURLToPath } from "node:url"

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const packagePath = path.join(packageRoot, "package.json")
const metadata = JSON.parse(await fs.readFile(packagePath, "utf8"))
const suffixes = [
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64-gnu",
  "linux-x64-gnu",
  "win32-x64-msvc",
]

metadata.optionalDependencies = Object.fromEntries(
  suffixes.map((suffix) => [
    `@eidos.space/graft-${suffix}`,
    metadata.version,
  ])
)

await fs.writeFile(packagePath, `${JSON.stringify(metadata, null, 2)}\n`)
console.log(`Prepared ${suffixes.length} optional packages for ${metadata.version}`)
