import { randomBytes, randomUUID, createHash, timingSafeEqual } from "node:crypto";
import {
  existsSync,
  readFileSync,
  writeFileSync,
  linkSync,
  unlinkSync,
  mkdirSync,
  openSync,
  fsyncSync,
  closeSync,
} from "node:fs";
import { join, dirname } from "node:path";
import { createApp, validateToken } from "./app.mjs";
import { Storage } from "./storage.mjs";
import { S3Blobs, s3Configuration } from "./s3.mjs";
import { setupPage } from "./setup-page.mjs";

const hash = (s) => createHash("sha256").update(s).digest();
const same = (a, b) => timingSafeEqual(hash(a), hash(b));
const headers = {
  "Cache-Control": "no-store",
  "Referrer-Policy": "no-referrer",
  "X-Content-Type-Options": "nosniff",
  "X-Frame-Options": "DENY",
};
const json = (value, status = 200) =>
  new Response(JSON.stringify(value), {
    status,
    headers: { ...headers, "Content-Type": "application/json" },
  });

// Publish only complete files, without replacing an existing installation.
function createFile(path, value) {
  const temporary = `${path}.${randomUUID()}.tmp`;
  writeFileSync(temporary, value, { flag: "wx", mode: 0o600, flush: true });
  try {
    linkSync(temporary, path);
    const directory = openSync(dirname(path), "r");
    try {
      fsyncSync(directory);
    } finally {
      closeSync(directory);
    }
  } finally {
    unlinkSync(temporary);
  }
}

function formStorage(input, env) {
  if (!input || typeof input !== "object" || Array.isArray(input))
    throw new Error("Invalid configuration");
  if (input.mode === "local") return null;
  if (input.mode !== "s3") throw new Error("Choose local disk or S3-compatible storage.");
  return s3Configuration(
    {
      GRAFT_STORAGE: "s3",
      GRAFT_S3_ENDPOINT: input.endpoint,
      GRAFT_S3_ALLOW_HTTP: env.GRAFT_S3_ALLOW_HTTP,
      GRAFT_S3_REGION: input.region,
      GRAFT_S3_BUCKET: input.bucket,
      GRAFT_S3_PREFIX: input.prefix || "graft/",
      GRAFT_S3_FORCE_PATH_STYLE: input.pathStyle === false ? "false" : "true",
      GRAFT_S3_ACCESS_KEY_ID_FILE: "accessKeyId",
      GRAFT_S3_SECRET_ACCESS_KEY_FILE: "secretAccessKey",
      ...(input.sessionToken ? { GRAFT_S3_SESSION_TOKEN_FILE: "sessionToken" } : {}),
    },
    (key) => (typeof input[key] === "string" ? input[key] : ""),
  );
}

async function checkStorage(config) {
  if (!config) return;
  const store = new S3Blobs(config);
  const id = randomUUID();
  const bytes = Buffer.from("Graft storage connection check");
  try {
    await store.health();
    try {
      await store.put(id, bytes, bytes.length);
      const result = await store.get(id, undefined, undefined, bytes.length);
      if (!Buffer.from(await new Response(result.body).arrayBuffer()).equals(bytes))
        throw new Error("Storage returned different data");
    } finally {
      await store.delete(id);
    }
  } catch {
    throw new Error(
      "S3 check failed. Check the endpoint, region, bucket and read/write/delete permissions. A temporary test object may need cleanup if deletion was denied.",
    );
  } finally {
    store.close();
  }
}

export async function createService({
  directory,
  origin,
  env = process.env,
  probe = checkStorage,
}) {
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const configPath = join(directory, "server-config.json");
  const claimPath = join(directory, "setup-token");
  const legacyPath = env.GRAFT_ACCESS_KEY_FILE ?? "/run/secrets/graft_access_key";
  let storage,
    app,
    blobStore,
    claim,
    busy = false,
    saved = false;
  let config;
  const activate = async (value) => {
    blobStore = value.s3 ? new S3Blobs(value.s3) : undefined;
    try {
      if (blobStore) await blobStore.health();
      storage = new Storage(join(directory, "graft.sqlite"), { blobStore });
      app = createApp({ storage, origin, token: value.token });
    } catch (error) {
      blobStore?.close();
      throw error;
    }
  };
  if (existsSync(configPath)) {
    config = JSON.parse(readFileSync(configPath, "utf8"));
    if (config.version !== 1) throw new Error("Unsupported server configuration version");
    validateToken(config.token);
    // Saved web configuration owns storage; external overrides must not silently change it.
    if (env.GRAFT_STORAGE || env.GRAFT_ACCESS_KEY_FILE)
      throw new Error(
        "Web setup is complete. Remove external storage/key overrides before starting.",
      );
    await activate(config);
  } else if (env.GRAFT_ACCESS_KEY_FILE || existsSync(legacyPath)) {
    config = {
      token: validateToken(readFileSync(legacyPath, "utf8").trim()),
      s3: s3Configuration(env),
    };
    await activate(config);
  } else {
    if (existsSync(join(directory, "graft.sqlite")))
      throw new Error(
        "Existing data has no access configuration. Restore the original key/configuration; setup will not reopen.",
      );
    if (env.GRAFT_STORAGE)
      throw new Error(
        "Use web setup for storage, or configure GRAFT_ACCESS_KEY_FILE for automated deployment.",
      );
    if (!existsSync(claimPath)) createFile(claimPath, randomBytes(32).toString("hex"));
    claim = validateToken(readFileSync(claimPath, "utf8").trim());
  }
  if (app && existsSync(claimPath)) unlinkSync(claimPath);
  return {
    get initialized() {
      return Boolean(app);
    },
    close() {
      storage?.close();
    },
    async request(request) {
      const path = new URL(request.url).pathname;
      if (request.method === "GET" && path === "/livez")
        return json({ status: "ok", setup_required: !app });
      if (request.method === "GET" && (path === "/setup" || (!app && path === "/"))) {
        const nonce = randomBytes(18).toString("base64");
        return new Response(setupPage({ complete: Boolean(app), nonce }), {
          headers: {
            ...headers,
            "Content-Type": "text/html; charset=utf-8",
            "Content-Security-Policy": `default-src 'none'; script-src 'nonce-${nonce}'; style-src 'nonce-${nonce}'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'`,
          },
        });
      }
      if (path === "/setup/status" || path === "/setup/test" || (path === "/setup" && request.method === "POST")) {
        if (app || saved)
          return json(
            {
              error:
                "Setup is complete. Use your saved access key or restart the server if it is not ready.",
            },
            409,
          );
        if (request.method !== "POST") return json({ error: "Use POST" }, 405);
        if (request.headers.get("origin") !== origin)
          return json({ error: "Open setup directly at the configured server address." }, 403);
        const supplied = request.headers.get("authorization") ?? "";
        if (!supplied.startsWith("Bearer ") || !same(supplied.slice(7), claim))
          return json({ error: "Open the initialization link from the server terminal." }, 401);
        // Check the terminal credential before showing any configuration fields.
        // This performs no storage probe and never consumes the claim.
        if (path === "/setup/status") return json({ status: "authorized" });
        if (!request.headers.get("content-type")?.startsWith("application/json"))
          return json({ error: "Expected JSON" }, 415);
        if (busy) return json({ error: "A storage check or setup is already in progress." }, 409);
        busy = true;
        try {
          const reader = request.body?.getReader();
          if (!reader) return json({ error: "Missing configuration" }, 400);
          const chunks = [];
          let length = 0;
          while (true) {
            const { value, done } = await reader.read();
            if (done) break;
            length += value.length;
            if (length > 16384) {
              await reader.cancel();
              return json({ error: "Configuration is too large" }, 413);
            }
            chunks.push(value);
          }
          const input = JSON.parse(Buffer.concat(chunks).toString("utf8"));
          const s3 = formStorage(input, env);
          await probe(s3);
          if (path === "/setup/test") return json({ status: "ok" });
          const value = { version: 1, token: randomBytes(32).toString("hex"), s3 };
          // Persist before opening the database. A crash can resume the same installation.
          createFile(configPath, JSON.stringify(value));
          saved = true;
          await activate(value);
          unlinkSync(claimPath);
          claim = undefined;
          return json({ origin, accessKey: value.token });
        } catch (error) {
          if (existsSync(configPath))
            return json(
              {
                error:
                  "Configuration was saved. Restart the container; retrieve the access key with the server access-key command if needed.",
              },
              503,
            );
          return json(
            { error: error instanceof SyntaxError ? "Invalid configuration" : error.message },
            400,
          );
        } finally {
          busy = false;
        }
      }
      if (app) return app(request);
      return json({ error: "setup_required", setup: "/setup" }, 503);
    },
  };
}
