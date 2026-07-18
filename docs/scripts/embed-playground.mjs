import { cp, rm, stat } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const docsRoot = resolve(fileURLToPath(new URL("..", import.meta.url)));
const source = resolve(docsRoot, "../web-demo/dist");
const target = resolve(docsRoot, "dist/playground");
const requiredFiles = [
  "index.html",
  "wasm/graft.js",
  "wasm/graft.wasm",
  "demo-assets/graft-app-state.png",
];

for (const path of requiredFiles) {
  const file = resolve(source, path);
  const metadata = await stat(file).catch(() => null);
  if (!metadata?.isFile() || metadata.size === 0) {
    throw new Error(`Playground build is missing ${path}. Run pnpm build:wasm first.`);
  }
}

await rm(target, { force: true, recursive: true });
await cp(source, target, { recursive: true });

console.log(`Embedded Playground at ${target}`);
