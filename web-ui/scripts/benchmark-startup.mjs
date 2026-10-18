import { spawn } from "node:child_process";
import { resolve } from "node:path";
import { performance } from "node:perf_hooks";

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(name);
  return index < 0 ? fallback : args[index + 1];
};
const repository = option("--repo");
const binary = resolve(option("--binary", "target/debug/graft"));
const runs = Number(option("--runs", "3"));
const budget = Number(option("--budget-ms", "Infinity"));
if (!repository || !Number.isInteger(runs) || runs < 1 || !(budget > 0)) {
  console.error("Usage: node web-ui/scripts/benchmark-startup.mjs --repo PATH [--binary PATH] [--runs 3] [--budget-ms 1500]");
  process.exit(1);
}

function measure() {
  return new Promise((resolveResult, reject) => {
    const start = performance.now();
    const child = spawn(binary, ["ui", resolve(repository), "--no-open"], { stdio: ["ignore", "pipe", "pipe"] });
    let loading;
    let ready;
    let output = "";
    let error = "";
    const timer = setTimeout(() => child.kill("SIGTERM"), 20_000);
    child.stdout.on("data", (chunk) => {
      output += chunk.toString();
      if (ready === undefined && output.includes("http://127.0.0.1:")) {
        ready = performance.now() - start;
        child.kill("SIGTERM");
      }
    });
    child.stderr.on("data", (chunk) => {
      loading ??= performance.now() - start;
      error += chunk.toString();
    });
    child.on("error", (cause) => { clearTimeout(timer); reject(cause); });
    child.on("close", () => {
      clearTimeout(timer);
      if (ready === undefined) reject(new Error(error.trim() || "Viewer did not become ready within 20 seconds"));
      else resolveResult({ loading_ms: Math.round(loading ?? 0), url_ready_ms: Math.round(ready) });
    });
  });
}

try {
  // Warm up filesystem caches and first-launch executable verification.
  await measure();
  const measurements = [];
  for (let run = 0; run < runs; run++) measurements.push(await measure());
  const ordered = measurements.map((item) => item.url_ready_ms).sort((a, b) => a - b);
  const median = ordered[Math.floor(ordered.length / 2)];
  console.log(JSON.stringify({ measurements, median_url_ms: median, within_budget: median <= budget }, null, 2));
  if (median > budget) process.exitCode = 1;
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
