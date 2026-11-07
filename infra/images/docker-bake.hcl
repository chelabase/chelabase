# Release images. `docker buildx bake -f infra/images/docker-bake.hcl` builds them all from one
# cargo build (the targets share server.Dockerfile's builder stage) and loads them into the local
# daemon. REPO is the image prefix, TAG the tag.
variable "REPO" { default = "chelabase" }
variable "TAG" { default = "latest" }
# REVISION is the git commit the images are built from (scripts/publish-images.sh sets it to HEAD);
# it goes into the org.opencontainers.image.revision label, which `--push-only` checks.
variable "REVISION" { default = "" }
variable "SOURCE" { default = "https://github.com/chelabase/chelabase" }
# RELEASE (any non-empty value; scripts/publish-images.sh sets it) adds the index annotations.
# Plain `just images` leaves them out: BuildKit refuses `index:` annotations for a
# single-manifest export, which is what `--load` into the classic image store makes.
variable "RELEASE" { default = "" }

# Index annotations: GHCR links a package to its repository from these, not from the labels.
# The revision one is left out while REVISION is empty.
function "index_annotations" {
  params = [release, source, revision]
  result = release == "" ? [] : concat(
    [
      "index:org.opencontainers.image.source=${source}",
      "index:org.opencontainers.image.licenses=Apache-2.0",
    ],
    revision == "" ? [] : ["index:org.opencontainers.image.revision=${revision}"],
  )
}

group "default" {
  targets = ["gateway", "orchestrator", "worker", "rest", "auth", "realtime", "storage", "functions", "service-runner", "init", "server", "studio", "identity", "functions-runtime"]
}

target "_app" {
  context    = "."
  dockerfile = "infra/images/server.Dockerfile"
  labels = {
    "org.opencontainers.image.revision" = "${REVISION}"
    "org.opencontainers.image.source"   = "${SOURCE}"
  }
  annotations = index_annotations(RELEASE, SOURCE, REVISION)
}

target "gateway" {
  inherits = ["_app"]
  target   = "gateway"
  tags     = ["${REPO}/gateway:${TAG}"]
}
target "orchestrator" {
  inherits = ["_app"]
  target   = "orchestrator"
  tags     = ["${REPO}/orchestrator:${TAG}"]
}
target "worker" {
  inherits = ["_app"]
  target   = "worker"
  tags     = ["${REPO}/worker:${TAG}"]
}
target "rest" {
  inherits = ["_app"]
  target   = "rest"
  tags     = ["${REPO}/rest:${TAG}"]
}
target "auth" {
  inherits = ["_app"]
  target   = "auth"
  tags     = ["${REPO}/auth:${TAG}"]
}
target "realtime" {
  inherits = ["_app"]
  target   = "realtime"
  tags     = ["${REPO}/realtime:${TAG}"]
}
# Object storage: in the default group (`just images`) since the base compose file runs it (m19);
# not in the release scripts yet (publish, pin, image-inputs): its first publish and pin are from
# `main` once the object-storage epic merges.
target "storage" {
  inherits = ["_app"]
  target   = "storage"
  tags     = ["${REPO}/storage:${TAG}"]
}
# Edge functions (m4b): the functions service. In the default group and the release scripts
# (publish, pin, image-inputs); its first pin is from `main` once the edge-functions epic merges.
target "functions" {
  inherits = ["_app"]
  target   = "functions"
  tags     = ["${REPO}/functions:${TAG}"]
}
# Edge functions (m10): the service runner (profile `services`). Unpublished until its first pin.
target "service-runner" {
  inherits = ["_app"]
  target   = "service-runner"
  tags     = ["${REPO}/service-runner:${TAG}"]
}
target "init" {
  inherits = ["_app"]
  target   = "init"
  tags     = ["${REPO}/init:${TAG}"]
}
# The all-in-one image, for the build-from-source override.
target "server" {
  inherits = ["_app"]
  target   = "all"
  tags     = ["${REPO}/server:${TAG}"]
}
target "studio" {
  context    = "."
  dockerfile = "infra/images/studio.Dockerfile"
  labels = {
    "org.opencontainers.image.revision" = "${REVISION}"
    "org.opencontainers.image.source"   = "${SOURCE}"
  }
  annotations = index_annotations(RELEASE, SOURCE, REVISION)
  tags       = ["${REPO}/studio:${TAG}"]
}
# The internal identity service (Node, Better Auth); its own Dockerfile, no cargo build.
# In the default group (`just images`); not in the release scripts yet (publish, pin, image-inputs).
target "identity" {
  context    = "."
  dockerfile = "infra/images/identity.Dockerfile"
  labels = {
    "org.opencontainers.image.revision" = "${REVISION}"
    "org.opencontainers.image.source"   = "${SOURCE}"
  }
  annotations = index_annotations(RELEASE, SOURCE, REVISION)
  tags        = ["${REPO}/identity:${TAG}"]
}
# The functions runtime (edge-functions m1): workerd and the supervisor; its own Dockerfile, no
# cargo build. In the default group (`just images`) and the release scripts (m4b).
target "functions-runtime" {
  context    = "."
  dockerfile = "infra/images/functions-runtime.Dockerfile"
  labels = {
    "org.opencontainers.image.revision" = "${REVISION}"
    "org.opencontainers.image.source"   = "${SOURCE}"
  }
  annotations = index_annotations(RELEASE, SOURCE, REVISION)
  tags        = ["${REPO}/functions-runtime:${TAG}"]
}
