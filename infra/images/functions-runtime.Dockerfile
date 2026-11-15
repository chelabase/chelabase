# The functions runtime (edge-functions m1): workerd, the supervisor worker and its watchdog.
# workerd is the official binary from npm (@cloudflare/workerd-linux-64 / -arm64), pinned by
# version and the registry's sha512 integrity. The runtime stage is distroless (glibc, no shell
# or package manager) plus a static busybox for the watchdog, running as nonroot (65532).
#
#   docker buildx bake -f infra/images/docker-bake.hcl functions-runtime

FROM debian:trixie-slim@sha256:89c1ea91e897f0fe0a3731595330bc45b8bdef4047464a07116f891a94054b31 AS workerd
ARG TARGETARCH
# Bump all three together; each sha512 is the version's `dist.integrity` in the npm registry.
ARG WORKERD_VERSION=1.20261023.1
ARG WORKERD_SHA512_AMD64=VHQ6jKFSihjR2+x1buby8XTdBLLJztVnYiB7OLOTYO8aDXB+c1Rd2410PdgFuFYc4AgFEk6PCJLyCLL5L1KBnw==
ARG WORKERD_SHA512_ARM64=ilpRHzfDN8YFRM5CLRKuFw40x/LJBS89yVdhdQmxehlVDuBqjKpJB++zPFbTgKRRFaNupgnXc8RHktXAIhgCmQ==
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) pkg=workerd-linux-64; sum="$WORKERD_SHA512_AMD64" ;; \
      arm64) pkg=workerd-linux-arm64; sum="$WORKERD_SHA512_ARM64" ;; \
      *) echo "no workerd for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/workerd.tgz "https://registry.npmjs.org/@cloudflare/$pkg/-/$pkg-$WORKERD_VERSION.tgz"; \
    [ "$(openssl dgst -sha512 -binary /tmp/workerd.tgz | base64 -w0)" = "$sum" ] \
      || { echo "workerd $WORKERD_VERSION ($pkg): sha512 mismatch" >&2; exit 1; }; \
    tar -xzf /tmp/workerd.tgz -C /tmp package/bin/workerd; \
    install -m 0755 /tmp/package/bin/workerd /usr/local/bin/workerd; \
    /usr/local/bin/workerd --version

FROM busybox:1.37.0-musl@sha256:f6a95d611e0a8780fabdb2aec43e31275b2720dfefa3e41b16835bfc42454478 AS busybox

FROM gcr.io/distroless/base-debian13:nonroot@sha256:9039a88709bcc2bacf5a2faac331d65dd6840f763f225ad9594cc0c67bcd0fba
COPY --from=workerd /usr/local/bin/workerd /usr/local/bin/workerd
COPY --from=busybox /bin/busybox /usr/local/bin/busybox
COPY infra/functions-runtime/config.capnp infra/functions-runtime/supervisor.js infra/functions-runtime/watchdog.sh /etc/functions-runtime/
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/busybox", "sh", "/etc/functions-runtime/watchdog.sh"]
CMD ["/usr/local/bin/workerd", "serve", "--experimental", "/etc/functions-runtime/config.capnp"]
