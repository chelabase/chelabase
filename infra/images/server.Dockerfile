# Release images for the Rust services (self-host). One builder stage runs one `cargo build`
# for every binary; each runtime target copies what it needs:
#   --target gateway|orchestrator|worker|rest|auth|realtime|storage|init   one binary, /usr/local/bin/<app>
#   --target all                                                   every binary (chelabase/server)
# `init` is the `chelabase-init` binary and keeps that name. The `chelabase` CLI is in none of them.
# The workbench does not use this file: it runs host-built debug binaries (infra/workbench).

# rust:1.99.0-slim-trixie (rust-toolchain.toml pins 1.99.0) and debian:trixie-slim, by digest.
FROM rust:1.99.0-slim-trixie@sha256:2752b332db73fdbb7dc576f06c82ed1f312005784ef913d7e04a28f5f55dc581 AS builder
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential cmake pkg-config clang git ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
      -p gateway -p orchestrator -p worker -p rest -p auth -p realtime -p chelabase-storage-service -p init \
 && mkdir /out \
 && for b in gateway orchestrator worker rest auth realtime storage chelabase-init; do \
      install -m 0755 "target/release/${b}" "/out/${b}"; \
    done

FROM debian:trixie-slim@sha256:89c1ea91e897f0fe0a3731595330bc45b8bdef4047464a07116f891a94054b31 AS base
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 10001 chelabase \
 && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin chelabase

# curl only where a healthcheck needs it: the services with an HTTP port.
FROM base AS base-http
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl \
 && rm -rf /var/lib/apt/lists/*

FROM base-http AS gateway
COPY --from=builder /out/gateway /usr/local/bin/gateway
USER 10001:10001
CMD ["gateway"]

FROM base-http AS orchestrator
COPY --from=builder /out/orchestrator /usr/local/bin/orchestrator
USER 10001:10001
CMD ["orchestrator"]

FROM base-http AS worker
COPY --from=builder /out/worker /usr/local/bin/worker
USER 10001:10001
CMD ["worker"]

FROM base-http AS rest
COPY --from=builder /out/rest /usr/local/bin/rest
USER 10001:10001
CMD ["rest"]

FROM base-http AS auth
COPY --from=builder /out/auth /usr/local/bin/auth
USER 10001:10001
CMD ["auth"]

FROM base-http AS realtime
COPY --from=builder /out/realtime /usr/local/bin/realtime
USER 10001:10001
CMD ["realtime"]

FROM base-http AS storage
COPY --from=builder /out/storage /usr/local/bin/storage
USER 10001:10001
CMD ["storage"]

FROM base AS init
COPY --from=builder /out/chelabase-init /usr/local/bin/chelabase-init
USER 10001:10001
CMD ["chelabase-init"]

FROM base-http AS all
COPY --from=builder /out/ /usr/local/bin/
USER 10001:10001
