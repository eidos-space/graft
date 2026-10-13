# `@eidos.space/graft-remote-cloudflare`

Cloudflare storage and authentication adapters for
`@eidos.space/graft-remote`.

This package contains the reusable Cloudflare-specific layer:

- `CloudflareRepositoryBackend` stores immutable objects in R2 and
  transactional metadata in a SQLite Durable Object;
- `openCloudflareRepository` opens the persisted plain or encrypted profile;
- `createEncryptedCloudflareRepository` atomically registers a new encrypted
  repository from the host's authorized account-management workflow;
- `RepositoryDurableObject` implements atomic create, CAS, CAD, and listing;
- `requireBearerToken` performs timing-safe bearer-token verification.

It does not create routes or export a Worker application. Use
`@eidos.space/graft-remote-hono` for Hono routing, or connect the backend to
another adapter.

## Install

```bash
npm install \
  @eidos.space/graft-remote-cloudflare \
  @eidos.space/graft-remote-hono \
  hono
```

## Compose a Worker

Generate the Worker `Env` from `wrangler.jsonc` with `wrangler types`, then pass
the generated bindings explicitly:

```ts
import { createGraftRemote } from "@eidos.space/graft-remote-hono";
import {
  openCloudflareRepository,
  GraftProtocolError,
  RepositoryDurableObject,
  requireBearerToken,
} from "@eidos.space/graft-remote-cloudflare";

export { RepositoryDurableObject };

type AppEnv = { Bindings: Env };

export default createGraftRemote<AppEnv>({
  async authenticate({ request, adapterContext }) {
    const token = adapterContext.env.GRAFT_REMOTE_TOKEN;
    if (!token) {
      throw new GraftProtocolError(503, "service_not_configured", "Token is not configured");
    }
    await requireBearerToken(request, token);
    return undefined;
  },

  backend({ adapterContext, repository }) {
    return openCloudflareRepository(
      {
        objects: adapterContext.env.OBJECTS,
        repositories: adapterContext.env.REPOSITORIES,
      },
      repository.id,
    );
  },
});
```

The host Worker owns binding names, secrets, authentication policy, and routing.
The package never calls the Cloudflare REST API; it uses R2 and Durable Object
bindings directly and streams immutable request and response bodies.

## Encrypted repositories

In an authenticated creation workflow, call
`createEncryptedCloudflareRepository(storage, repositoryId, uuid)` with the
new canonical v4 UUID. The host must authorize creation for that account and
repository. An existing plain profile, a different UUID, or existing plain data
is rejected; the API does not convert a repository in place. Normal requests use
`openCloudflareRepository` and cannot select or change the stored profile.

Encrypted config, refs and HEAD use exact-byte Durable Object CAS. Immutable
ciphertext is staged in private R2 keys and exposed through an atomic Durable
Object record. Multipart sessions retain UUID, opaque object key, encryption
upload ID, total bytes, full ciphertext SHA256 and part size. Resume requires an
exact match. Complete verifies the assembled bytes before publication; direct
and multipart writers compete on the same create-only record. An interrupted or
failed upload never appears in raw reads or listings. A completion with an
unknown outcome must be reconciled before abort; a verified invalid upload may
be aborted and restarted. Published objects must not be expired by a bucket rule
covering the `encrypted-staging` prefix: it also holds retained published data.

Public opaque hint topology is indexed in Durable Object SQL in the same
transaction that exposes each hint. Planning survives isolate eviction without
scanning all R2 hints. The planner bounds graph work and selected packs, detects
missing nodes, cycles and conflicting parents, and returns an unavailable plan
for encrypted primitive fallback. Clients still authenticate hints and verify
the actual pack topology and full dependency closure.

After restoring storage, the host may run
`rebuildCloudflareEncryptedRouting(storage, repositoryId)` as an explicit
maintenance operation. It rebuilds from published hint objects in bounded pages;
concurrent new hints remain indexed atomically. It never creates or changes refs,
and primitive reads remain usable while an incomplete graph yields HTTP 422.

See `services/graft-remote-cloudflare` for a deployable verification service,
Wrangler configuration, and end-to-end Workers runtime tests.
