# Graft Remote Cloudflare Verification Service

This directory contains the deployable verification service for the
[Graft remote service protocol](../../docs/src/content/docs/docs/reference/remote-protocol.mdx).
The implementation-independent HTTP behavior lives in the publishable
[`@eidos.space/graft-remote`](../../packages/graft-remote) core package. The
[`@eidos.space/graft-remote-hono`](../../packages/graft-remote-hono) adapter
connects that engine to Hono. The reusable Cloudflare storage and authentication
layer lives in
[`@eidos.space/graft-remote-cloudflare`](../../packages/graft-remote-cloudflare).
This service only composes the packages with generated bindings, Wrangler
configuration, and end-to-end runtime tests.

The implementation is intentionally split by data semantics:

- the core package validates repository URLs, negotiates protocol version 1,
  and implements all protocol operations without a framework dependency;
- the Hono adapter maps routes and Context objects into the core engine;
- the Cloudflare package performs bearer-token authentication and opens the
  binding-backed repository backend;
- a SQLite Durable Object stores `HEAD` and `refs/**`, so CAS and CAD execute as
  single conditional SQL statements;
- R2 stores immutable repository objects, packs, external files, log commits,
  and segments. Upload and download bodies are streamed through the Worker.

The public repository URL is Git-like:

```text
https://graft.example.com/acme/archive
```

Graft also accepts the explicit form
`graft+https://graft.example.com/acme/archive`. The Worker keeps the early
`/api/graft/v1/repos/acme/archive` path as a compatibility alias.

## Requirements

- Node.js 20, 22, or 24+
- pnpm 10
- a Cloudflare account with Workers, Durable Objects, and R2 enabled

## Local Development

```bash
pnpm install --frozen-lockfile
cp .dev.vars.example .dev.vars
pnpm types
pnpm dev
```

Use the local service with the explicit insecure transport:

```bash
export GRAFT_REMOTE_TOKEN='replace-with-a-local-development-token'
graft remote add origin 'graft+http://127.0.0.1:8787/acme/archive'
```

`.dev.vars` is ignored by Git. Do not commit real tokens.

## Test and Validate

```bash
pnpm check
pnpm test
pnpm exec wrangler deploy --dry-run
```

Tests run inside the Workers runtime with local SQLite Durable Object and R2
bindings. They cover authentication, version negotiation, create-only writes,
Range responses, concurrent CAS, CAD, repository isolation, Durable Object
restart persistence, cursor-paginated listing, legacy routing, and path
validation.

## Deploy

The checked-in `wrangler.jsonc` uses these bucket names:

```text
graft-remote-objects
graft-remote-objects-preview
```

Create them once, or change both names in the config:

```bash
pnpm exec wrangler r2 bucket create graft-remote-objects
pnpm exec wrangler r2 bucket create graft-remote-objects-preview
pnpm exec wrangler secret put GRAFT_REMOTE_TOKEN
pnpm deploy
```

Attach a Worker custom domain such as `graft.example.com`, then configure a
remote:

```bash
export GRAFT_REMOTE_TOKEN='the-token-entered-with-wrangler'
graft remote add origin 'https://graft.example.com/acme/archive'
graft ls-remote origin
```

The reference adapter uses one deployment-wide bearer token. For multi-user or
multi-tenant deployments, replace its `authenticate` and `authorize` callbacks
with an identity provider and repository ACL lookup. Those policies belong to
the host application and are deliberately not fixed by the protocol package.
Never put a token in `wrangler.jsonc` or in a remote URL.

## Operational Properties

- R2 objects use a repository-prefixed key and conditional `If-None-Match: *`
  writes.
- Mutable metadata is limited to 16 KiB. Object keys are limited to 768 UTF-8
  bytes and `locks/**` is reserved.
- Unconditional protocol writes and deletes are rejected for immutable keys.
- Every response is `Cache-Control: no-store` because repositories are private
  by default.
- List responses use bounded cursor pages (100 paths by default, at most 500).
  The Rust client follows `next_cursor` until it has the complete, sorted
  result.
- A single HTTP upload is still subject to the Cloudflare account's incoming
  request-body limit. Streaming avoids Worker memory buffering but does not
  bypass that edge limit.

The Worker compatibility date is pinned to the newest date supported by the
checked-in Workers test runtime. Update Wrangler, generated types, the date, and
tests together.
