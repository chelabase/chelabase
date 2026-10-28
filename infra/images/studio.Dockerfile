# Studio's release image: `vite build`, served statically on port 3000 (the port the gateway's
# Studio upstream expects) with an SPA fallback. No build-time ids: Studio reads its config at
# run time.
FROM node:24-trixie-slim@sha256:89c5244bc22a9bc3287d89ebc7e49b9cb5bb84cd40e6175ac3421d5a9f68b95a AS build
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0 COREPACK_ENABLE_AUTO_PIN=0 CI=true
RUN corepack enable && corepack prepare pnpm@12.6.0 --activate
WORKDIR /src
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY apps ./apps
COPY packages ./packages
COPY openapi ./openapi
# vite.config.ts reads the entry defaults from here.
COPY infra/compose/.env.example ./infra/compose/.env.example
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile --filter "@chelabase/studio..." \
 && pnpm --filter @chelabase/studio build

FROM caddy:2-alpine@sha256:eb4d35b7452d1ab6171fbdbe8d9275b2301dad6dad92115d91e19fb9b24d510f
COPY infra/images/studio.Caddyfile /etc/caddy/Caddyfile
COPY --from=build /src/apps/studio/dist /srv
# XDG dirs are writable by the non-root user; Caddy's admin API and autosave stay off in the Caddyfile.
USER 10001:10001
EXPOSE 3000
CMD ["caddy", "run", "--config", "/etc/caddy/Caddyfile", "--adapter", "caddyfile"]
