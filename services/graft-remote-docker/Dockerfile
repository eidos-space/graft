# Build from the repository root. Pin this base to a reviewed digest for deployment.
FROM node:24-bookworm-slim@sha256:c86ecead9e55a5210ac1727de38a4ca840bf03dcaf34dc9749addbf383b1920c AS build
WORKDIR /app
RUN npm install --global pnpm@10.14.0
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY packages/graft-remote/ packages/graft-remote/
RUN pnpm --filter @eidos.space/graft-remote install --frozen-lockfile && pnpm --filter @eidos.space/graft-remote build

FROM node:24-bookworm-slim@sha256:c86ecead9e55a5210ac1727de38a4ca840bf03dcaf34dc9749addbf383b1920c
ENV NODE_ENV=production
WORKDIR /app
COPY --from=build /app/packages/graft-remote/dist/ packages/graft-remote/dist/
COPY packages/graft-remote/package.json packages/graft-remote/package.json
COPY packages/graft-remote/LICENSE packages/graft-remote/LICENSE
COPY LICENSE-APACHE LICENSE-MIT /usr/share/licenses/graft/
COPY services/graft-remote-docker/*.mjs services/graft-remote-docker/
RUN mkdir /data && chown node:node /data
USER node
EXPOSE 8787
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 CMD ["node", "-e", "fetch('http://127.0.0.1:8787/healthz').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"]
CMD ["node", "services/graft-remote-docker/server.mjs"]
