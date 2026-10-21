# The workbench image (W2): the runtime of a stack's app tier (orchestrator, worker, gateway,
# auth, rest, realtime run the checkout's host-built debug binaries), Studio, docs and landing
# (the Vite dev servers, from the checkout's node_modules) and the test runner. Workbench-only; not a
# self-hosting image. `chelabase workbench up` builds it as chelabase-workbench:<hash>, <hash>
# being the first 12 hex characters of this file's SHA-256.
#
# Compose sets the user (the host's uid:gid), so there is no USER line, and everything a process
# writes at run time goes under HOME=/tmp or the mounted checkout and state dir.
FROM ubuntu:26.04@sha256:f144425ff09be612d6d9ad965196e9cdc23dae1f42110a8a11a3e9a8198759f7

SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl jq postgresql-client openssl iproute2 xz-utils gnupg \
 && rm -rf /var/lib/apt/lists/*

# The Docker CLI and the Compose plugin (docker-ce-cli, docker-compose-plugin), from Docker's apt repo: the worker, the orchestrator and
# the runner talk to the host's daemon through the mounted socket. The repo key is trusted only
# when it is exactly one primary key with Docker's published fingerprint.
ARG DOCKER_KEY_FPR=9DC858229FC7DD38854AE2D88D81803C0EBFCD88
RUN install -m 0755 -d /etc/apt/keyrings \
 && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /tmp/docker.asc \
 && fprs="$(GNUPGHOME="$(mktemp -d)" gpg --batch --show-keys --with-colons /tmp/docker.asc \
      | awk -F: '$1 == "pub" { p = 1; next } $1 == "fpr" && p { print $10; p = 0 }')" \
 && if [[ "${fprs}" != "${DOCKER_KEY_FPR}" ]]; then \
      echo "Docker's apt key has fingerprint(s) '${fprs}', expected ${DOCKER_KEY_FPR}" >&2; exit 1; \
    fi \
 && install -m 0644 /tmp/docker.asc /etc/apt/keyrings/docker.asc \
 && rm -f /tmp/docker.asc \
 && . /etc/os-release \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu ${VERSION_CODENAME} stable" \
      > /etc/apt/sources.list.d/docker.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends docker-ce-cli docker-compose-plugin \
 && rm -rf /var/lib/apt/lists/*

# Node, the host's version (so a checkout's node_modules match), from nodejs.org's tarball for
# the build architecture. SHASUMS256.txt.asc must carry a good signature by one of the active
# Node.js releasers (the node README's "Release keys", pinned below by primary fingerprint; key
# files from nodejs/release-keys at a pinned commit), and the tarball's line is checked in the
# signed content only.
ARG NODE_VERSION=24.21.0
ARG NODE_RELEASE_KEYS_COMMIT=481637f813e912c4aa3622d7964ab426c97b8e8d
ARG NODE_RELEASE_KEYS="\
5BE8A3F6C8A5C01D106C0AD820B1A390B168D356 \
DD792F5973C6DE52C432CBDAC77ABFA00DDBF2B7 \
CC68F5A3106FF448322E48ED27F5E38D5B0A215F \
890C08DB8579162FEE0DF9DB8BEAB4DFCF555EF4 \
C82FA3AE1CBEDC6BE46B9360C43CEC45C17AB93C \
108F52B48DB57BB0CC439B2997B01419BD92F80A \
655F3B5C1FB3FA8D1A0CA6BDE4A7D232B936D2FD \
A363A499291CBBC940DD62E41F10027AF002F8B0"
RUN dpkg_arch="$(dpkg --print-architecture)" \
 && case "${dpkg_arch}" in \
      amd64) node_arch=x64 ;; \
      arm64) node_arch=arm64 ;; \
      *) echo "no Node.js build for architecture ${dpkg_arch}" >&2; exit 1 ;; \
    esac \
 && work="$(mktemp -d)" \
 && cd "${work}" \
 && export GNUPGHOME="${work}/gnupg" \
 && install -m 0700 -d "${GNUPGHOME}" \
 && for fpr in ${NODE_RELEASE_KEYS}; do \
      curl -fsSL "https://raw.githubusercontent.com/nodejs/release-keys/${NODE_RELEASE_KEYS_COMMIT}/keys/${fpr}.asc" \
        | gpg --batch --quiet --import; \
    done \
 && tarball="node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
 && curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/${tarball}" \
 && curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt.asc" \
 && gpg --batch --status-file gpg.status --output shasums.verified --verify SHASUMS256.txt.asc \
 && signer="$(awk '$2 == "VALIDSIG" { print $NF }' gpg.status)" \
 && if [[ -z "${signer}" || " ${NODE_RELEASE_KEYS} " != *" ${signer} "* ]]; then \
      echo "SHASUMS256.txt.asc isn't signed by a pinned Node.js release key (signer '${signer}')" >&2; exit 1; \
    fi \
 && grep " ${tarball}\$" shasums.verified | sha256sum -c - \
 && tar -xJf "${tarball}" -C /usr/local --strip-components=1 --no-same-owner \
 && cd / \
 && rm -rf "${work}" \
 && node --version

# pnpm at the repo's packageManager, through corepack. COREPACK_HOME is readable by any uid, so
# the host user Compose runs as finds the prepared pnpm without downloading it.
ENV COREPACK_HOME=/opt/corepack \
    COREPACK_ENABLE_DOWNLOAD_PROMPT=0 \
    COREPACK_ENABLE_AUTO_PIN=0
RUN corepack enable \
 && corepack prepare pnpm@12.6.0 --activate \
 && chmod -R a+rX "${COREPACK_HOME}" \
 && pnpm --version

ENV HOME=/tmp
