//! What the runner needs from a container engine. Docker is the one backend today
//! ([`crate::docker::DockerBackend`]); the trait speaks the runner's own types, so a Kubernetes
//! backend can implement it later. Every method acts on what it's given: the label guard is the
//! runner's ([`crate::runner::Runner`]), before any call that names a container.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::spec::{ImageRef, Limits};

/// A container as the runner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// The engine's full id (never the name a caller gave).
    pub id: String,
    pub labels: HashMap<String, String>,
    /// The engine's state word (`running`, `exited`, `created`, …).
    pub state: String,
}

impl Container {
    pub fn running(&self) -> bool {
        self.state == "running" || self.state == "restarting"
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }
}

/// What the runner reads from an image before it runs it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageFacts {
    /// The image's `USER`, if any.
    pub user: Option<String>,
    /// The image's `VOLUME` paths: each gets a tmpfs, never an anonymous volume on the host.
    pub volumes: Vec<String>,
}

/// A container to create, already decided by the runner: every field here is the runner's,
/// none is passed through from a caller.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerPlan {
    pub name: String,
    pub image: ImageRef,
    /// `uid` or `uid:gid`, never 0.
    pub user: String,
    pub env: Vec<String>,
    pub labels: HashMap<String, String>,
    pub limits: Limits,
    /// Absolute paths that get a size-limited tmpfs (`/tmp` and the image's volumes).
    pub tmpfs: Vec<String>,
}

#[async_trait]
pub trait ContainerBackend: Send + Sync + 'static {
    /// The container `id` (an id or a name), or `None` when there is none.
    async fn inspect(&self, id: &str) -> anyhow::Result<Option<Container>>;
    /// Every container (running or not) labelled with `stack` and with a service label. The
    /// runner checks the labels again on what comes back.
    async fn list(&self, stack: &str) -> anyhow::Result<Vec<Container>>;
    /// The image's facts, pulling it first (with the engine's credentials) when it's missing.
    async fn image(&self, image: &ImageRef) -> anyhow::Result<ImageFacts>;
    /// Creates the container on `network` and returns its id; it isn't started.
    async fn create(&self, plan: &ContainerPlan, network: &str) -> anyhow::Result<String>;
    async fn start(&self, id: &str) -> anyhow::Result<()>;
    async fn stop(&self, id: &str) -> anyhow::Result<()>;
    async fn remove(&self, id: &str) -> anyhow::Result<()>;
    /// The last `tail` lines of stdout and stderr.
    async fn logs(&self, id: &str, tail: usize) -> anyhow::Result<String>;
}
