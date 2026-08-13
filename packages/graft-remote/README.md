# `@eidos.space/graft-remote`

Framework-neutral types and protocol engine for the Graft remote service
protocol.

This package owns protocol negotiation, validation, range reads, cursor
pagination, mutation semantics, error responses, and the repository backend
contract. It depends only on standard Fetch API types. HTTP framework routing
belongs in a separate adapter.

## Install

```bash
npm install @eidos.space/graft-remote
```

Use `@eidos.space/graft-remote-hono` when the host application uses Hono, and
`@eidos.space/graft-remote-cloudflare` for the reusable R2/Durable Object
backend.

## Create a transport adapter

A router extracts route parameters and passes them, the standard `Request`, and
its own framework or runtime context to the protocol engine:

```ts
import {
  GraftProtocolError,
  createGraftRemoteHandler,
  type GraftRouteParameters,
} from "@eidos.space/graft-remote";

interface RuntimeContext {
  repositories: RepositoryStore;
}

interface User {
  id: string;
}

const handleRemote = createGraftRemoteHandler<RuntimeContext, User>({
  limits: {
    maxRequestBytes: 64 * 1024 * 1024,
    multipartPartBytes: 16 * 1024 * 1024,
  },
  async authenticate({ request }) {
    return await authenticateRequest(request);
  },

  async authorize({ action, principal, repository }) {
    if (
      principal === undefined ||
      !(await canAccess(principal, action, repository))
    ) {
      throw new GraftProtocolError(
        403,
        "forbidden",
        "Repository access denied",
      );
    }
  },

  backend({ adapterContext, repository }) {
    return adapterContext.repositories.open(repository.id);
  },
});

export function dispatchRemote(
  request: Request,
  route: GraftRouteParameters,
  runtime: RuntimeContext,
): Promise<Response> {
  return handleRemote({ request, route, adapterContext: runtime });
}
```

The adapter is responsible only for matching these route values:

- `namespace`
- `repository`
- optional `operation`
- optional slash-containing `objectPath`

The engine validates all values and returns a standard `Response`, including
protocol error responses.

## Implement the backend

Implement `GraftRepositoryBackend` for the storage system you already use:

```ts
interface GraftRepositoryBackend {
  head(path: string): MaybePromise<GraftObjectMetadata | null>;
  get(path: string, range?: GraftByteRange): MaybePromise<GraftObject | null>;
  put(path: string, value: Uint8Array<ArrayBuffer>): MaybePromise<void>;
  delete(path: string): MaybePromise<void>;
  putIfAbsent(
    path: string,
    value: Uint8Array<ArrayBuffer> | ReadableStream<Uint8Array>,
    kind: "transactional" | "immutable",
    options?: { contentLength?: number },
  ): MaybePromise<boolean>;
  compareAndSwap(
    path: string,
    expected: Uint8Array<ArrayBuffer> | undefined,
    replacement: Uint8Array<ArrayBuffer>,
  ): MaybePromise<boolean>;
  compareAndDelete(
    path: string,
    expected: Uint8Array<ArrayBuffer> | undefined,
  ): MaybePromise<boolean>;
  list(query: GraftListQuery): MaybePromise<GraftListResult>;
  multipart?: GraftMultipartBackend;
}
```

Backend guarantees:

- `compareAndSwap` and `compareAndDelete` are atomic.
- `putIfAbsent` atomically creates only when the path is absent.
- `list` returns at most `query.limit` paths, sorted by UTF-8 byte order, after
  `query.after`, and restricted to `query.prefix`. It may also return matching
  `entries` with object sizes so aggregate downloads can declare their exact
  transfer length without a second metadata lookup.
- Immutable request bodies remain streams when the adapter storage supports it.
- `upload-bundle` lists immutable keys and streams each `get` body into one
  clone response without buffering the repository in memory. Its exact framed
  length is exposed through `Content-Length` and
  `x-graft-bundle-total-bytes`, allowing clone clients to report total size,
  percentage, and estimated time remaining before the body finishes.
- `options.contentLength` is the exact length of a framed immutable body, such
  as each object in a `receive-pack` or `receive-bundle` request.
- `multipart` optionally stores one logical immutable object through resumable
  parts. The protocol engine advertises it as `multipart-object`, validates
  every part against `limits.multipartPartBytes`, and keeps the final object at
  the original repository path.
- Repository instances are isolated by `repository.id`.

## Authentication and authorization

The engine does not prescribe an account system. `authenticate` returns the
host application's principal; `authorize` receives that principal with the
`discover`, `read`, or `write` action, concrete operation, repository, and
optional object path.

This supports bearer tokens, sessions, signed requests, identity-aware proxies,
mutual TLS, or an SSH gateway without placing account or ACL formats in the
wire protocol.

`repositoryId` can map a visible namespace/name to an internal tenant ID.
Returning `null` from `backend` reports a missing repository. `onError` can
report failures without taking over protocol response formatting.

## Development

From the Graft repository root:

```bash
pnpm --filter @eidos.space/graft-remote check
pnpm --filter @eidos.space/graft-remote test
pnpm --filter @eidos.space/graft-remote build
```
