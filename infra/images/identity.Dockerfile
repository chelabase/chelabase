# The internal identity service: Better Auth behind our own JSON API, run by Node directly
# from its TypeScript sources (type stripping). Node 26 is the active LTS.
FROM node:26.13.0-trixie-slim@sha256:e4db4c914bf8ad241bd24804fc25d272d89bad97e68cdc8a1414862790a02c39 AS build
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ENV CI=true
# Node 25+ ships without corepack.
RUN npm install --global pnpm@12.6.0
WORKDIR /src
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY services/identity ./services/identity
# Production dependencies only, flattened into a self-contained directory.
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm --filter @chelabase/identity deploy --legacy --prod /out \
 && rm -rf /out/tests /out/bench /out/vitest.config.ts

FROM node:26.13.0-trixie-slim@sha256:e4db4c914bf8ad241bd24804fc25d272d89bad97e68cdc8a1414862790a02c39
ENV NODE_ENV=production
WORKDIR /app
COPY --from=build /out ./
USER node
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --retries=3 \
  CMD ["node", "-e", "fetch('http://127.0.0.1:8080/health').then(r=>process.exit(r.ok?0:1),()=>process.exit(1))"]
CMD ["node", "src/main.ts"]
