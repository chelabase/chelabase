# Hook relay image: a host-built binary on a minimal base. Build with `just hook-relay-build`,
# which stages `cheladb-hook-relay` alone in the build context.
#
# The binary is a glibc build from the host toolchain, so the base must carry a glibc at
# least as new as the host's: ubuntu:26.04 (latest LTS, glibc 2.43), pinned by digest.
# Rebuild the image (and re-run the recipe) after changing the host OS.
FROM ubuntu:26.04@sha256:3595d7fc4286a33fad0fd853a4063e654287a9c3787437d7937c94ca3f7a804e
COPY cheladb-hook-relay /usr/local/bin/cheladb-hook-relay
USER 65534:65534
EXPOSE 8099
ENTRYPOINT ["/usr/local/bin/cheladb-hook-relay"]
CMD ["--listen", "0.0.0.0:8099", "--slots-dir", "/slots"]
