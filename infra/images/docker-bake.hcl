# Release images. `docker buildx bake -f infra/images/docker-bake.hcl` builds them all from one
# cargo build (the targets share server.Dockerfile's builder stage) and loads them into the local
# daemon. REPO is the image prefix, TAG the tag.
variable "REPO" { default = "chelabase" }
variable "TAG" { default = "latest" }

group "default" {
  targets = ["gateway", "orchestrator", "worker", "rest", "auth", "realtime", "init", "server", "studio"]
}

target "_app" {
  context    = "."
  dockerfile = "infra/images/server.Dockerfile"
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
  tags       = ["${REPO}/studio:${TAG}"]
}
