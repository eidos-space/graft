import { readFile, stat } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const demoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(demoRoot, "..");
const cargoManifest = await readFile(
  resolve(repositoryRoot, "crates/graft-cli/Cargo.toml"),
  "utf8",
);
const sourceVersion = cargoManifest.match(/^version = "([^"]+)"$/m)?.[1];
if (!sourceVersion) throw new Error("Could not read the graft-cli version");

const runtimeDirectory = resolve(demoRoot, "public/wasm");
const runtimeManifestPath = resolve(runtimeDirectory, "version.json");
const runtimeManifest = JSON.parse(await readFile(runtimeManifestPath, "utf8"));
if (runtimeManifest.version !== sourceVersion) {
  throw new Error(
    `Playground Wasm is ${runtimeManifest.version ?? "unversioned"}, but graft-cli is ${sourceVersion}. Run pnpm build:wasm.`,
  );
}

for (const fileName of ["graft.js", "graft.wasm"]) {
  const metadata = await stat(resolve(runtimeDirectory, fileName));
  if (!metadata.isFile() || metadata.size === 0) {
    throw new Error(`Playground runtime is missing ${fileName}. Run pnpm build:wasm.`);
  }
}

for (const relativePath of [
  "docs/src/content/docs/docs/quickstart/playground.mdx",
  "docs/src/content/docs/zh/docs/quickstart/playground.mdx",
]) {
  const contents = await readFile(resolve(repositoryRoot, relativePath), "utf8");
  if (!contents.includes(`v${sourceVersion}`)) {
    throw new Error(`${relativePath} does not describe Graft v${sourceVersion}`);
  }
}

console.log(`Playground runtime matches Graft v${sourceVersion} (${runtimeManifest.build})`);
