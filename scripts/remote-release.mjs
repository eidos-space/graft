import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);

export const remotePackages = [
  {
    name: "@eidos.space/graft-remote",
    directory: "packages/graft-remote",
    dependencies: [],
  },
  {
    name: "@eidos.space/graft-remote-hono",
    directory: "packages/graft-remote-hono",
    dependencies: ["@eidos.space/graft-remote"],
  },
  {
    name: "@eidos.space/graft-remote-cloudflare",
    directory: "packages/graft-remote-cloudflare",
    dependencies: ["@eidos.space/graft-remote"],
  },
];

const visibilityPollIntervalMs = 5_000;
const visibilityPollAttempts = 120;

export function releaseTagForVersion(version) {
  return version.includes("-") ? "next" : "latest";
}

export function archiveName(packageName, version) {
  return `${packageName.slice(1).replace("/", "-")}-${version}.tgz`;
}

export function validatePackageMetadata(metadataByName, expectedVersion) {
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(expectedVersion)) {
    throw new Error(
      `Remote package version is not valid SemVer: ${expectedVersion}`,
    );
  }

  for (const releasePackage of remotePackages) {
    const metadata = metadataByName.get(releasePackage.name);
    if (metadata === undefined) {
      throw new Error(`Missing package metadata for ${releasePackage.name}`);
    }
    if (metadata.version !== expectedVersion) {
      throw new Error(
        `${releasePackage.name} has version ${metadata.version}, expected ${expectedVersion}`,
      );
    }
    if (metadata.publishConfig?.access !== "public") {
      throw new Error(`${releasePackage.name} must publish with public access`);
    }
    if (metadata.publishConfig?.provenance !== true) {
      throw new Error(`${releasePackage.name} must publish with provenance`);
    }
    for (const dependency of releasePackage.dependencies) {
      if (metadata.dependencies?.[dependency] !== "workspace:^") {
        throw new Error(
          `${releasePackage.name} must depend on ${dependency} via workspace:^`,
        );
      }
    }
  }
}

async function readSourceMetadata(expectedVersion) {
  const metadataByName = new Map();
  for (const releasePackage of remotePackages) {
    const metadata = await readJson(
      path.join(repositoryRoot, releasePackage.directory, "package.json"),
    );
    metadataByName.set(metadata.name, metadata);
  }
  validatePackageMetadata(metadataByName, expectedVersion);
  return metadataByName;
}

async function validate(expectedVersion) {
  await readSourceMetadata(expectedVersion);
  console.log(
    `Validated ${remotePackages.length} Remote packages at ${expectedVersion}`,
  );
}

async function prepare(expectedVersion, outputDirectory) {
  await readSourceMetadata(expectedVersion);
  const releaseRoot = path.resolve(repositoryRoot, outputDirectory);
  await fs.rm(releaseRoot, { recursive: true, force: true });
  await fs.mkdir(releaseRoot, { recursive: true });

  for (const releasePackage of remotePackages) {
    await command("pnpm", [
      "--filter",
      releasePackage.name,
      "pack",
      "--pack-destination",
      releaseRoot,
    ]);
    const archive = path.join(
      releaseRoot,
      archiveName(releasePackage.name, expectedVersion),
    );
    await fs.access(archive);
  }

  const checksumLines = [];
  for (const releasePackage of remotePackages) {
    const filename = archiveName(releasePackage.name, expectedVersion);
    const bytes = await fs.readFile(path.join(releaseRoot, filename));
    checksumLines.push(
      `${createHash("sha256").update(bytes).digest("hex")}  ${filename}`,
    );
  }
  await fs.writeFile(
    path.join(releaseRoot, "SHA256SUMS"),
    `${checksumLines.join("\n")}\n`,
  );
  console.log(`Prepared Remote release assets in ${releaseRoot}`);
}

async function verify(expectedVersion, outputDirectory) {
  await readSourceMetadata(expectedVersion);
  const releaseRoot = path.resolve(repositoryRoot, outputDirectory);
  const expectedArchives = remotePackages.map(({ name }) =>
    archiveName(name, expectedVersion),
  );
  const actualFiles = (await fs.readdir(releaseRoot)).sort();
  const expectedFiles = [...expectedArchives, "SHA256SUMS"].sort();
  if (JSON.stringify(actualFiles) !== JSON.stringify(expectedFiles)) {
    throw new Error(
      `Remote release assets differ: found ${actualFiles.join(", ")}, expected ${expectedFiles.join(", ")}`,
    );
  }

  const checksumLines = (
    await fs.readFile(path.join(releaseRoot, "SHA256SUMS"), "utf8")
  )
    .trim()
    .split("\n");
  const checksums = new Map(
    checksumLines.map((line) => {
      const match = line.match(/^([0-9a-f]{64})  (.+)$/);
      if (match === null) throw new Error(`Invalid SHA256SUMS line: ${line}`);
      return [match[2], match[1]];
    }),
  );

  for (const releasePackage of remotePackages) {
    const filename = archiveName(releasePackage.name, expectedVersion);
    const archive = path.join(releaseRoot, filename);
    const bytes = await fs.readFile(archive);
    const digest = createHash("sha256").update(bytes).digest("hex");
    if (checksums.get(filename) !== digest) {
      throw new Error(`Checksum mismatch for ${filename}`);
    }

    const listing = await command("tar", ["-tzf", archive], { capture: true });
    const files = listing.stdout.trim().split("\n");
    for (const required of [
      "package/package.json",
      "package/README.md",
      "package/LICENSE",
      "package/dist/index.js",
      "package/dist/index.d.ts",
    ]) {
      if (!files.includes(required))
        throw new Error(`${filename} is missing ${required}`);
    }
    if (
      files.some(
        (file) =>
          file.startsWith("package/src/") || file.includes("node_modules"),
      )
    ) {
      throw new Error(`${filename} contains source or node_modules files`);
    }

    const packedManifest = await command(
      "tar",
      ["-xOf", archive, "package/package.json"],
      { capture: true },
    );
    const metadata = JSON.parse(packedManifest.stdout);
    if (
      metadata.name !== releasePackage.name ||
      metadata.version !== expectedVersion
    ) {
      throw new Error(`${filename} contains unexpected package identity`);
    }
    for (const dependency of releasePackage.dependencies) {
      if (metadata.dependencies?.[dependency] !== `^${expectedVersion}`) {
        throw new Error(
          `${filename} must pin ${dependency} to ^${expectedVersion} after packing`,
        );
      }
    }
  }

  console.log(
    `Verified ${remotePackages.length} Remote package archives at ${expectedVersion}`,
  );
}

async function publish(expectedVersion, outputDirectory) {
  await verify(expectedVersion, outputDirectory);
  const releaseRoot = path.resolve(repositoryRoot, outputDirectory);
  const releaseTag = releaseTagForVersion(expectedVersion);
  const dryRun = process.env.GRAFT_REMOTE_RELEASE_DRY_RUN === "1";

  for (const releasePackage of remotePackages) {
    if (await published(releasePackage.name, expectedVersion)) {
      console.log(
        `Already published: ${releasePackage.name}@${expectedVersion}`,
      );
      continue;
    }
    if (dryRun) {
      console.log(`Would publish: ${releasePackage.name}@${expectedVersion}`);
      continue;
    }
    await command("npm", [
      "publish",
      path.join(releaseRoot, archiveName(releasePackage.name, expectedVersion)),
      "--access",
      "public",
      "--tag",
      releaseTag,
      "--ignore-scripts",
      "--provenance",
    ]);
    await waitUntilPublished(releasePackage.name, expectedVersion);
  }
}

async function published(name, version) {
  const result = await command(
    "npm",
    ["view", `${name}@${version}`, "version", "--json"],
    { allowFailure: true, capture: true },
  );
  if (result.code === 0) return JSON.parse(result.stdout) === version;
  if (result.stderr.includes("E404")) return false;
  throw new Error(
    `Could not query ${name}@${version}: ${result.stderr.trim()}`,
  );
}

async function waitUntilPublished(name, version) {
  for (let attempt = 1; attempt <= visibilityPollAttempts; attempt += 1) {
    if (await published(name, version)) {
      console.log(`Registry visible: ${name}@${version}`);
      return;
    }
    if (attempt % 12 === 0) {
      console.log(`Waiting for npm registry visibility: ${name}@${version}`);
    }
    if (attempt < visibilityPollAttempts) {
      await new Promise((resolve) =>
        setTimeout(resolve, visibilityPollIntervalMs),
      );
    }
  }
  throw new Error(`${name}@${version} was not visible after ten minutes`);
}

async function readJson(filename) {
  return JSON.parse(await fs.readFile(filename, "utf8"));
}

function command(executable, arguments_, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(executable, arguments_, {
      cwd: repositoryRoot,
      env: process.env,
      stdio: options.capture ? ["ignore", "pipe", "pipe"] : "inherit",
    });
    let stdout = "";
    let stderr = "";
    if (options.capture) {
      child.stdout.setEncoding("utf8");
      child.stderr.setEncoding("utf8");
      child.stdout.on("data", (chunk) => (stdout += chunk));
      child.stderr.on("data", (chunk) => (stderr += chunk));
    }
    child.on("error", reject);
    child.on("exit", (code) => {
      const result = { code: code ?? 1, stdout, stderr };
      if (result.code === 0 || options.allowFailure) resolve(result);
      else
        reject(
          new Error(
            `${executable} ${arguments_.join(" ")} exited ${result.code}`,
          ),
        );
    });
  });
}

async function main() {
  const [
    operation,
    expectedVersion,
    outputDirectory = "release-assets/graft-remote",
  ] = process.argv.slice(2);
  if (operation === undefined || expectedVersion === undefined) {
    throw new Error(
      "Usage: node scripts/remote-release.mjs <validate|prepare|verify|publish> VERSION [OUTPUT_DIR]",
    );
  }
  if (operation === "validate") await validate(expectedVersion);
  else if (operation === "prepare")
    await prepare(expectedVersion, outputDirectory);
  else if (operation === "verify")
    await verify(expectedVersion, outputDirectory);
  else if (operation === "publish")
    await publish(expectedVersion, outputDirectory);
  else throw new Error(`Unknown Remote release operation: ${operation}`);
}

if (path.resolve(process.argv[1] ?? "") === fileURLToPath(import.meta.url)) {
  await main();
}
