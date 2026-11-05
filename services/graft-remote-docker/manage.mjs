import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { validateOrigin } from "./app.mjs";

const directory = process.env.GRAFT_DATA_DIR ?? "/data";
const config = join(directory, "server-config.json");
const command = process.argv[2];
if (command === "setup-link") {
  if (existsSync(config))
    throw new Error("Setup is complete. Use access-key to retrieve your key.");
  const token = readFileSync(join(directory, "setup-token"), "utf8").trim();
  const origin = validateOrigin(process.env.GRAFT_PUBLIC_ORIGIN ?? "http://127.0.0.1:8787");
  console.log(`${origin}/setup#token=${encodeURIComponent(token)}`);
} else if (command === "access-key") {
  // Explicit operator action; never emit credentials in normal server logs.
  const token = existsSync(config)
    ? JSON.parse(readFileSync(config, "utf8")).token
    : readFileSync(
        process.env.GRAFT_ACCESS_KEY_FILE ?? "/run/secrets/graft_access_key",
        "utf8",
      ).trim();
  console.log(token);
} else {
  console.error("Usage: node services/graft-remote-docker/manage.mjs setup-link|access-key");
  process.exitCode = 1;
}
