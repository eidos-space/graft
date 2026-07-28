import { spawn } from "node:child_process"
import fs from "node:fs/promises"
import path from "node:path"
import { fileURLToPath } from "node:url"

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const rootPackage = JSON.parse(
  await fs.readFile(path.join(packageRoot, "package.json"), "utf8")
)
const packageDirectories = (await fs.readdir(path.join(packageRoot, "npm")))
  .sort()
  .map((directory) => path.join(packageRoot, "npm", directory))
const releaseTag = rootPackage.version.includes("-") ? "next" : "latest"
const dryRun = process.env.GRAFT_SDK_RELEASE_DRY_RUN === "1"

for (const directory of [...packageDirectories, packageRoot]) {
  const metadata = JSON.parse(
    await fs.readFile(path.join(directory, "package.json"), "utf8")
  )
  if (metadata.version !== rootPackage.version) {
    throw new Error(`${metadata.name} has version ${metadata.version}, expected ${rootPackage.version}`)
  }
  if (await published(metadata.name, metadata.version)) {
    console.log(`Already published: ${metadata.name}@${metadata.version}`)
    continue
  }
  if (dryRun) {
    console.log(`Would publish: ${metadata.name}@${metadata.version}`)
    continue
  }
  await command("npm", [
    "publish",
    directory,
    "--access",
    "public",
    "--tag",
    releaseTag,
    "--ignore-scripts",
    "--provenance",
  ])
  await waitUntilPublished(metadata.name, metadata.version)
}

async function published(name, version) {
  const result = await command(
    "npm",
    ["view", `${name}@${version}`, "version", "--json"],
    { allowFailure: true, quiet: true }
  )
  if (result.code === 0) return JSON.parse(result.stdout) === version
  if (result.stderr.includes("E404")) return false
  throw new Error(`Could not query ${name}@${version}: ${result.stderr.trim()}`)
}

async function waitUntilPublished(name, version) {
  for (let attempt = 0; attempt < 12; attempt += 1) {
    if (await published(name, version)) return
    await new Promise((resolve) => setTimeout(resolve, 5_000))
  }
  throw new Error(`${name}@${version} was not visible after publication`)
}

function command(executable, arguments_, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(executable, arguments_, {
      cwd: packageRoot,
      env: process.env,
      stdio: options.quiet ? ["ignore", "pipe", "pipe"] : "inherit",
    })
    let stdout = ""
    let stderr = ""
    if (options.quiet) {
      child.stdout.setEncoding("utf8")
      child.stderr.setEncoding("utf8")
      child.stdout.on("data", (chunk) => (stdout += chunk))
      child.stderr.on("data", (chunk) => (stderr += chunk))
    }
    child.on("error", reject)
    child.on("exit", (code) => {
      const result = { code: code ?? 1, stdout, stderr }
      if (result.code === 0 || options.allowFailure) resolve(result)
      else reject(new Error(`${executable} ${arguments_.join(" ")} exited ${result.code}`))
    })
  })
}
