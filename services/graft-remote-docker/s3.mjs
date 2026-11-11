import { readFileSync } from "node:fs";
import {
  S3Client,
  HeadBucketCommand,
  HeadObjectCommand,
  GetObjectCommand,
  PutObjectCommand,
  DeleteObjectCommand,
} from "@aws-sdk/client-s3";
import { GraftProtocolError } from "../../packages/graft-remote/dist/index.js";
import { Upload } from "@aws-sdk/lib-storage";

const missing = (error) => error?.$metadata?.httpStatusCode === 404;

export function s3Configuration(
  env = process.env,
  readSecret = (path) => readFileSync(path, "utf8"),
) {
  const mode = env.GRAFT_STORAGE ?? "local";
  if (!["local", "s3"].includes(mode)) throw new Error("GRAFT_STORAGE must be local or s3");
  if (mode === "local") return null;
  const required = (key) => {
    if (!env[key]?.trim()) throw new Error(`${key} is required for S3 storage`);
    return env[key].trim();
  };
  const endpoint = new URL(required("GRAFT_S3_ENDPOINT"));
  if (
    endpoint.username ||
    endpoint.password ||
    endpoint.search ||
    endpoint.hash ||
    endpoint.pathname !== "/" ||
    !["https:", "http:"].includes(endpoint.protocol) ||
    (endpoint.protocol === "http:" && env.GRAFT_S3_ALLOW_HTTP !== "true")
  )
    throw new Error(
      "S3 endpoint must be an HTTPS origin; HTTP requires explicit GRAFT_S3_ALLOW_HTTP=true for local testing",
    );
  const bucket = required("GRAFT_S3_BUCKET");
  if (!/^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$/.test(bucket)) throw new Error("Invalid S3 bucket name");
  const prefix = env.GRAFT_S3_PREFIX ?? "graft/";
  if (!/^[A-Za-z0-9_-]+(?:\/[A-Za-z0-9_-]+)*\/$/.test(prefix))
    throw new Error(
      "GRAFT_S3_PREFIX must be a nonempty slash-terminated path of letters, digits, _ or -",
    );
  const region = required("GRAFT_S3_REGION");
  const forcePathStyle = env.GRAFT_S3_FORCE_PATH_STYLE ?? "true";
  if (!["true", "false"].includes(forcePathStyle))
    throw new Error("Invalid GRAFT_S3_FORCE_PATH_STYLE");
  const secret = (key) => {
    const value = readSecret(required(key)).trim();
    if (!value || /\s/.test(value)) throw new Error(`Invalid secret file for ${key}`);
    return value;
  };
  return {
    endpoint: endpoint.origin,
    region,
    bucket,
    prefix,
    forcePathStyle: forcePathStyle === "true",
    credentials: {
      accessKeyId: secret("GRAFT_S3_ACCESS_KEY_ID_FILE"),
      secretAccessKey: secret("GRAFT_S3_SECRET_ACCESS_KEY_FILE"),
      ...(env.GRAFT_S3_SESSION_TOKEN_FILE
        ? { sessionToken: secret("GRAFT_S3_SESSION_TOKEN_FILE") }
        : {}),
    },
  };
}

/** Each SQLite-published blob owns a random key; no user path becomes an S3 key. */
export class S3Blobs {
  constructor(config) {
    this.bucket = config.bucket;
    this.prefix = config.prefix;
    // Never persist credentials; this identity prevents an accidental bucket/mode switch.
    this.identity = JSON.stringify({
      mode: "s3",
      endpoint: config.endpoint,
      region: config.region,
      bucket: config.bucket,
      prefix: config.prefix,
    });
    this.client = new S3Client({
      ...config,
      maxAttempts: 3,
      requestChecksumCalculation: "WHEN_REQUIRED",
      responseChecksumValidation: "WHEN_REQUIRED",
    });
  }
  key(id) {
    if (!/^[a-f0-9-]{36}$/.test(id)) throw new Error("Invalid internal blob id");
    return `${this.prefix}${id}`;
  }
  async health() {
    await this.client.send(new HeadBucketCommand({ Bucket: this.bucket }), {
      abortSignal: AbortSignal.timeout(15_000),
    });
  }
  async put(id, body, length, signal) {
    const abort = AbortSignal.any([AbortSignal.timeout(120_000), ...(signal ? [signal] : [])]);
    await this.client.send(
      new PutObjectCommand({
        Bucket: this.bucket,
        Key: this.key(id),
        Body: body,
        ContentLength: length,
      }),
      { abortSignal: abort },
    );
  }
  async upload(id, stream, length, signal) {
    const upload = new Upload({
      client: this.client,
      params: {
        Bucket: this.bucket,
        Key: this.key(id),
        Body: stream,
        ContentLength: length,
      },
      queueSize: 2,
      partSize: 16 * 1024 * 1024,
      leavePartsOnError: false,
    });
    const abort = AbortSignal.any([AbortSignal.timeout(30 * 60_000), ...(signal ? [signal] : [])]);
    const cancel = () => {
      stream.destroy();
      void upload.abort().catch(() => {});
    };
    abort.addEventListener("abort", cancel, { once: true });
    try {
      abort.throwIfAborted();
      await upload.done();
      abort.throwIfAborted();
    } finally {
      abort.removeEventListener("abort", cancel);
      stream.destroy();
    }
  }
  async get(id, range, signal, expectedSize) {
    let response;
    try {
      response = await this.client.send(
        new GetObjectCommand({
          Bucket: this.bucket,
          Key: this.key(id),
          ...(range ? { Range: `bytes=${range.start}-${range.end}` } : {}),
        }),
        {
          abortSignal: AbortSignal.any([
            AbortSignal.timeout(30 * 60_000),
            ...(signal ? [signal] : []),
          ]),
        },
      );
    } catch (error) {
      if (missing(error))
        throw new GraftProtocolError(404, "object_not_found", "Stored object is missing");
      throw new GraftProtocolError(
        503,
        "object_store_unavailable",
        "Object storage is unavailable",
        { "Retry-After": "1" },
      );
    }
    if (
      !response.Body ||
      (!range && expectedSize !== undefined && response.ContentLength !== expectedSize) ||
      (range &&
        (response.ContentLength !== range.end - range.start + 1 ||
          (expectedSize !== undefined
            ? response.ContentRange !== `bytes ${range.start}-${range.end}/${expectedSize}`
            : !response.ContentRange?.startsWith(`bytes ${range.start}-${range.end}/`))))
    ) {
      response.Body?.destroy();
      throw new GraftProtocolError(
        409,
        "object_length_mismatch",
        "S3 returned an inconsistent byte range",
      );
    }
    return {
      body: response.Body.transformToWebStream(),
      size: response.ContentLength,
    };
  }
  async delete(id) {
    // B2 and versioned S3 need VersionId deletion to reclaim bytes, not a delete marker.
    // All versions of this random key belong exclusively to this blob's upload retries.
    const input = { Bucket: this.bucket, Key: this.key(id) };
    for (let attempt = 0; attempt < 16; attempt++) {
      let head;
      try {
        head = await this.client.send(new HeadObjectCommand(input), {
          abortSignal: AbortSignal.timeout(30_000),
        });
      } catch (error) {
        if (missing(error)) return;
        throw error;
      }
      await this.client.send(
        new DeleteObjectCommand({
          ...input,
          ...(head.VersionId ? { VersionId: head.VersionId } : {}),
        }),
        { abortSignal: AbortSignal.timeout(30_000) },
      );
      if (!head.VersionId) return;
    }
    throw new Error("Blob version cleanup deferred");
  }
  close() {
    this.client.destroy();
  }
}
