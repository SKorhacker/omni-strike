# syntax=docker/dockerfile:1.7
#
# Omni Strike demo UI (Next.js, web/). Build from the REPO ROOT:
#   docker build -f deploy/web.Dockerfile -t omni-strike/web:latest .
#   docker build -f deploy/web.Dockerfile --build-arg OA_BACKEND_URL=http://oa-server:8080 ...
#
# OA_BACKEND_URL feeds the rewrites in web/next.config.ts (/api/v1/* and /api/health/* -> backend).
# Next.js resolves rewrites at BUILD time (routes manifest), so the value is a build arg; it is
# also exported at run time for completeness. Changing the backend URL = rebuild this image.

ARG NODE_IMAGE=node:24-bookworm-slim

FROM ${NODE_IMAGE} AS deps
ENV PNPM_HOME=/pnpm \
    NEXT_TELEMETRY_DISABLED=1
ENV PATH="${PNPM_HOME}:${PATH}"
# pnpm version comes from "packageManager" in web/package.json (pnpm@10.x).
RUN corepack enable
WORKDIR /web
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN --mount=type=cache,id=pnpm-store,target=/pnpm/store \
    pnpm config set store-dir /pnpm/store \
 && pnpm install --frozen-lockfile

FROM deps AS builder
ARG OA_BACKEND_URL=http://oa-server:8080
ENV OA_BACKEND_URL=${OA_BACKEND_URL} \
    NODE_ENV=production
COPY web/ ./
RUN pnpm build

FROM ${NODE_IMAGE} AS runtime
ARG OA_BACKEND_URL=http://oa-server:8080
LABEL org.opencontainers.image.title="omni-strike-web" \
      org.opencontainers.image.description="Omni Strike demo UI (Next.js)" \
      io.omnistrike.backend-url="${OA_BACKEND_URL}"
RUN apt-get update \
 && apt-get install -y --no-install-recommends tini \
 && rm -rf /var/lib/apt/lists/*
ENV NODE_ENV=production \
    NEXT_TELEMETRY_DISABLED=1 \
    OA_BACKEND_URL=${OA_BACKEND_URL} \
    PORT=3000 \
    HOSTNAME=0.0.0.0
WORKDIR /web
# Full node_modules (not --prod): `next start` loads next.config.ts, which needs the TS toolchain.
COPY --from=builder --chown=node:node /web/package.json /web/next.config.ts ./
COPY --from=builder --chown=node:node /web/node_modules ./node_modules
COPY --from=builder --chown=node:node /web/.next ./.next
USER node
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
  CMD node -e "fetch('http://127.0.0.1:3000/').then(r=>process.exit(r.status<500?0:1)).catch(()=>process.exit(1))"
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["node", "node_modules/next/dist/bin/next", "start", "-p", "3000", "-H", "0.0.0.0"]
