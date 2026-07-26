import { GraftProtocolError, createGraftRemote } from "@eidos.space/graft-remote-hono";
import {
  CloudflareRepositoryBackend,
  requireBearerToken,
} from "@eidos.space/graft-remote-cloudflare";

export { RepositoryDurableObject } from "@eidos.space/graft-remote-cloudflare";

type AppEnv = { Bindings: Env };

const app = createGraftRemote<AppEnv>({
  legacyRoutes: true,
  async authenticate({ request, adapterContext }): Promise<undefined> {
    if (!adapterContext.env.GRAFT_REMOTE_TOKEN) {
      throw new GraftProtocolError(
        503,
        "service_not_configured",
        "GRAFT_REMOTE_TOKEN is not configured",
      );
    }
    await requireBearerToken(request, adapterContext.env.GRAFT_REMOTE_TOKEN);
    return undefined;
  },
  backend({ adapterContext, repository }) {
    return new CloudflareRepositoryBackend(
      {
        objects: adapterContext.env.OBJECTS,
        repositories: adapterContext.env.REPOSITORIES,
      },
      repository.id,
    );
  },
});

export default app;
