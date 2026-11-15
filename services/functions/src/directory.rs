//! Where a function's serving state comes from: the metadata database, behind [`Directory`] so
//! the service's tests use a fake.

use std::collections::BTreeMap;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chelabase_http::outbound::Url;
use chelabase_metadata::{
    ContainerTarget, DeploymentRecord, FunctionAuth, FunctionKind, MetadataClient, MetadataError,
    ResolvedFunction, ServiceTarget,
};
use chelabase_types::{BranchId, DatabaseId};
use uuid::Uuid;
use zeroize::Zeroizing;

/// The env the service always sets; a secret of the same name is overridden.
pub const ENV_URL: &str = "CHELABASE_URL";
pub const ENV_BRANCH_ID: &str = "CHELABASE_BRANCH_ID";
pub const ENV_SERVICE_KEY: &str = "CHELABASE_SERVICE_KEY";

/// A function as a branch serves it, with its env (decrypted secrets, wiped on drop).
pub struct Resolution {
    pub database_id: Uuid,
    pub function_id: Uuid,
    pub bundle_sha256: String,
    pub auth: FunctionAuth,
    pub enabled: bool,
    /// `x-chelabase-env`: base64 of the env's JSON object.
    pub env_header: Zeroizing<String>,
    /// m9: an external service's target; `None` for a function.
    pub service: Option<External>,
}

/// m9: where an external service is (`spec = {backend: "external", url, health_path}`) and its
/// signing secret (wiped on drop; `None` when it has none, and every call answers 503).
///
/// m11: a container service (`spec = {backend: "container", image, port, …}`) has `container`
/// set to its port, and `url` is a placeholder never dialled: its address is the one the
/// service runner reports ([`crate::services::container`]).
pub struct External {
    pub deployment_id: Uuid,
    pub url: Url,
    pub secret: Option<Zeroizing<String>>,
    pub container: Option<u16>,
}

impl External {
    /// The target a resolved service's deployment names: an external URL or a container.
    pub fn from_spec(deployment_id: Uuid, spec: &serde_json::Value) -> Option<Self> {
        match spec.get("backend")?.as_str()? {
            "external" => Some(External {
                deployment_id,
                url: Url::parse(spec.get("url")?.as_str()?).ok()?,
                secret: None,
                container: None,
            }),
            "container" => {
                let port = u16::try_from(spec.get("port")?.as_u64()?).ok()?;
                Some(External {
                    deployment_id,
                    url: Url::parse("http://container.invalid/").expect("a valid URL"),
                    secret: None,
                    container: Some(port),
                })
            }
            _ => None,
        }
    }
}

impl fmt::Debug for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolution")
            .field("database_id", &self.database_id)
            .field("function_id", &self.function_id)
            .field("bundle_sha256", &self.bundle_sha256)
            .field("auth", &self.auth)
            .field("enabled", &self.enabled)
            .field("env_header", &"<redacted>")
            .field("service", &self.service.as_ref().map(|e| (e.deployment_id, e.url.as_str())))
            .finish()
    }
}

/// What a resolution is made of. `secrets` and `service_key` are wiped on drop.
pub struct Parts {
    pub database_id: Uuid,
    pub function_id: Uuid,
    pub bundle_sha256: String,
    pub auth: FunctionAuth,
    pub enabled: bool,
    pub secrets: Vec<(String, Zeroizing<String>)>,
    pub service_key: Option<Zeroizing<String>>,
    pub service: Option<External>,
}

impl Resolution {
    /// The function's env: its secrets, then [`ENV_URL`] (`public_url`), [`ENV_BRANCH_ID`]
    /// and, with a live run-as key, [`ENV_SERVICE_KEY`] (absent otherwise, even as a secret).
    pub fn build(parts: Parts, branch: Uuid, public_url: &str) -> Self {
        let branch = branch.to_string();
        let mut env: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, value) in &parts.secrets {
            env.insert(name, value);
        }
        env.remove(ENV_SERVICE_KEY);
        env.insert(ENV_URL, public_url);
        env.insert(ENV_BRANCH_ID, &branch);
        if let Some(key) = &parts.service_key {
            env.insert(ENV_SERVICE_KEY, key);
        }
        let json = Zeroizing::new(serde_json::to_vec(&env).expect("a map of strings serialises"));
        Self {
            database_id: parts.database_id,
            function_id: parts.function_id,
            bundle_sha256: parts.bundle_sha256,
            auth: parts.auth,
            enabled: parts.enabled,
            env_header: Zeroizing::new(STANDARD.encode(&*json)),
            service: parts.service,
        }
    }
}

#[async_trait::async_trait]
pub trait Directory: Send + Sync + 'static {
    /// The function `name` on `branch`, enabled or not; `None` when it has no deployment
    /// there, isn't a bundle function, or the branch or its database is going away.
    async fn resolve(&self, branch: Uuid, name: &str) -> anyhow::Result<Option<Parts>>;
    /// Sets the function's `last_error` on the branch.
    async fn record_error(&self, branch: Uuid, function: Uuid, error: &str) -> anyhow::Result<()>;
    /// The database exists and isn't deleting or deleted.
    async fn database_live(&self, database: Uuid) -> anyhow::Result<bool>;
    /// m9: the external services' deployments to health-check.
    async fn service_targets(&self) -> anyhow::Result<Vec<ServiceTarget>> {
        Ok(Vec::new())
    }
    /// m9: records a deployment's health.
    async fn record_health(&self, _deployment: Uuid, _healthy: bool) -> anyhow::Result<()> {
        Ok(())
    }
    /// m11: the container services enabled on live branches, to ensure and health-check.
    async fn container_targets(&self) -> anyhow::Result<Vec<ContainerTarget>> {
        Ok(Vec::new())
    }
    /// m11: a container service's env on `branch`: its secrets, then [`ENV_URL`] and
    /// [`ENV_BRANCH_ID`] (decrypted, wiped on drop).
    async fn container_env(
        &self,
        _branch: Uuid,
        _function: Uuid,
    ) -> anyhow::Result<BTreeMap<String, Zeroizing<String>>> {
        Ok(BTreeMap::new())
    }
    /// m11: a deployment's spec, when it is the function's.
    async fn deployment_spec(
        &self,
        _function: Uuid,
        _deployment: Uuid,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

/// What [`Metadata`] needs from the metadata database: [`MetadataClient`]'s calls, behind a
/// trait so a test can fake them.
#[async_trait::async_trait]
pub trait MetadataPort: Send + Sync + 'static {
    async fn resolve(
        &self,
        branch: BranchId,
        name: &str,
    ) -> Result<Option<ResolvedFunction>, MetadataError>;
    /// The branch's database.
    async fn branch_database(&self, branch: BranchId) -> Result<Option<DatabaseId>, MetadataError>;
    async fn decrypt_secrets(
        &self,
        branch: BranchId,
        function: Uuid,
    ) -> Result<Vec<(String, String)>, MetadataError>;
    /// The function's live managed run-as key, opened: `(key id, secret)`.
    async fn managed_run_as_key(
        &self,
        branch: BranchId,
        function: Uuid,
    ) -> Result<Option<(Uuid, String)>, MetadataError>;
    async fn set_function_last_error(
        &self,
        branch: BranchId,
        function: Uuid,
        error: Option<&str>,
    ) -> Result<(), MetadataError>;
    async fn live_database(&self, database: DatabaseId) -> Result<bool, MetadataError>;
    /// m9: a service's signing secret, opened.
    async fn service_signing_secret(&self, _function: Uuid) -> Result<Option<String>, MetadataError> {
        Ok(None)
    }
    async fn list_service_targets(&self) -> Result<Vec<ServiceTarget>, MetadataError> {
        Ok(Vec::new())
    }
    async fn set_service_health(&self, _deployment: Uuid, _healthy: bool) -> Result<(), MetadataError> {
        Ok(())
    }
    async fn list_container_targets(&self) -> Result<Vec<ContainerTarget>, MetadataError> {
        Ok(Vec::new())
    }
    async fn list_deployments(&self, _function: Uuid) -> Result<Vec<DeploymentRecord>, MetadataError> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl MetadataPort for MetadataClient {
    async fn resolve(
        &self,
        branch: BranchId,
        name: &str,
    ) -> Result<Option<ResolvedFunction>, MetadataError> {
        MetadataClient::resolve(self, branch, name).await
    }
    async fn branch_database(&self, branch: BranchId) -> Result<Option<DatabaseId>, MetadataError> {
        Ok(self.get_branch(branch).await?.map(|b| b.database_id))
    }
    async fn decrypt_secrets(
        &self,
        branch: BranchId,
        function: Uuid,
    ) -> Result<Vec<(String, String)>, MetadataError> {
        MetadataClient::decrypt_secrets(self, branch, function).await
    }
    async fn managed_run_as_key(
        &self,
        branch: BranchId,
        function: Uuid,
    ) -> Result<Option<(Uuid, String)>, MetadataError> {
        MetadataClient::managed_run_as_key(self, branch, function).await
    }
    async fn set_function_last_error(
        &self,
        branch: BranchId,
        function: Uuid,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        MetadataClient::set_function_last_error(self, branch, function, error).await
    }
    async fn live_database(&self, database: DatabaseId) -> Result<bool, MetadataError> {
        MetadataClient::live_database(self, database).await
    }
    async fn service_signing_secret(&self, function: Uuid) -> Result<Option<String>, MetadataError> {
        MetadataClient::service_signing_secret(self, function).await
    }
    async fn list_service_targets(&self) -> Result<Vec<ServiceTarget>, MetadataError> {
        MetadataClient::list_service_targets(self).await
    }
    async fn set_service_health(&self, deployment: Uuid, healthy: bool) -> Result<(), MetadataError> {
        MetadataClient::set_service_health(self, deployment, healthy).await
    }
    async fn list_container_targets(&self) -> Result<Vec<ContainerTarget>, MetadataError> {
        MetadataClient::list_container_targets(self).await
    }
    async fn list_deployments(&self, function: Uuid) -> Result<Vec<DeploymentRecord>, MetadataError> {
        MetadataClient::list_deployments(self, function).await
    }
}

/// The metadata database. Its client carries the secrets key (`with_secrets_key`): function
/// secrets and the managed run-as key open with it.
pub struct Metadata<P = MetadataClient> {
    pub client: P,
    /// m11: a container service's `CHELABASE_URL`.
    pub public_url: String,
}

#[async_trait::async_trait]
impl<P: MetadataPort> Directory for Metadata<P> {
    async fn resolve(&self, branch: Uuid, name: &str) -> anyhow::Result<Option<Parts>> {
        let Some(f) = self.client.resolve(BranchId(branch), name).await? else {
            return Ok(None);
        };
        let Some(database) = self.client.branch_database(BranchId(branch)).await? else {
            return Ok(None);
        };
        if f.kind == FunctionKind::Service {
            let Some(mut external) = External::from_spec(f.deployment_id, &f.spec) else {
                return Ok(None);
            };
            if f.enabled {
                external.secret = self
                    .client
                    .service_signing_secret(f.function_id)
                    .await?
                    .map(Zeroizing::new);
            }
            return Ok(Some(Parts {
                database_id: database.as_uuid(),
                function_id: f.function_id,
                bundle_sha256: String::new(),
                auth: f.auth,
                enabled: f.enabled,
                secrets: Vec::new(),
                service_key: None,
                service: Some(external),
            }));
        }
        let Some(sha) = f.bundle_sha256 else {
            return Ok(None);
        };
        // A disabled function is never run: its secrets stay sealed.
        let (secrets, service_key) = if f.enabled {
            let secrets = self
                .client
                .decrypt_secrets(BranchId(branch), f.function_id)
                .await?
                .into_iter()
                .map(|(name, value)| (name, Zeroizing::new(value)))
                .collect();
            let key = match f.run_as_key_id {
                Some(_) => self.service_key(branch, f.function_id).await?,
                None => None,
            };
            (secrets, key)
        } else {
            (Vec::new(), None)
        };
        Ok(Some(Parts {
            database_id: database.as_uuid(),
            function_id: f.function_id,
            bundle_sha256: sha,
            auth: f.auth,
            enabled: f.enabled,
            secrets,
            service_key,
            service: None,
        }))
    }

    async fn record_error(&self, branch: Uuid, function: Uuid, error: &str) -> anyhow::Result<()> {
        // Capped at MAX_LAST_ERROR_CHARS, and written only while the branch is live.
        self.client
            .set_function_last_error(BranchId(branch), function, Some(error))
            .await?;
        Ok(())
    }

    async fn database_live(&self, database: Uuid) -> anyhow::Result<bool> {
        Ok(self.client.live_database(DatabaseId(database)).await?)
    }

    async fn service_targets(&self) -> anyhow::Result<Vec<ServiceTarget>> {
        Ok(self.client.list_service_targets().await?)
    }

    async fn record_health(&self, deployment: Uuid, healthy: bool) -> anyhow::Result<()> {
        Ok(self.client.set_service_health(deployment, healthy).await?)
    }

    async fn container_targets(&self) -> anyhow::Result<Vec<ContainerTarget>> {
        Ok(self.client.list_container_targets().await?)
    }

    async fn container_env(
        &self,
        branch: Uuid,
        function: Uuid,
    ) -> anyhow::Result<BTreeMap<String, Zeroizing<String>>> {
        let mut env: BTreeMap<String, Zeroizing<String>> = self
            .client
            .decrypt_secrets(BranchId(branch), function)
            .await?
            .into_iter()
            .map(|(name, value)| (name, Zeroizing::new(value)))
            .collect();
        env.insert(ENV_URL.into(), Zeroizing::new(self.public_url.clone()));
        env.insert(ENV_BRANCH_ID.into(), Zeroizing::new(branch.to_string()));
        Ok(env)
    }

    async fn deployment_spec(
        &self,
        function: Uuid,
        deployment: Uuid,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        Ok(self
            .client
            .list_deployments(function)
            .await?
            .into_iter()
            .find(|d| d.id == deployment && d.function_id == function)
            .map(|d| d.spec))
    }
}

impl<P: MetadataPort> Metadata<P> {
    /// The function's live managed key on the branch, opened (`run_as_role`'s key): none once
    /// it is revoked, or when it isn't managed by this function.
    async fn service_key(
        &self,
        branch: Uuid,
        function: Uuid,
    ) -> anyhow::Result<Option<Zeroizing<String>>> {
        let key = self
            .client
            .managed_run_as_key(BranchId(branch), function)
            .await?;
        Ok(key.map(|(_, secret)| Zeroizing::new(secret)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(secrets: &[(&str, &str)], service_key: Option<&str>) -> Parts {
        Parts {
            database_id: Uuid::nil(),
            function_id: Uuid::nil(),
            bundle_sha256: "a".repeat(64),
            auth: FunctionAuth::Key,
            enabled: true,
            secrets: secrets
                .iter()
                .map(|(n, v)| (n.to_string(), Zeroizing::new(v.to_string())))
                .collect(),
            service_key: service_key.map(|k| Zeroizing::new(k.to_string())),
            service: None,
        }
    }

    fn env(r: &Resolution) -> serde_json::Value {
        serde_json::from_slice(&STANDARD.decode(r.env_header.as_bytes()).unwrap()).unwrap()
    }

    /// The metadata database as a fake: one function, its managed key (if live), and the
    /// `last_error` writes.
    struct FakePort {
        function_id: Uuid,
        database: DatabaseId,
        run_as_key_id: Option<Uuid>,
        managed_key: Option<(Uuid, String)>,
        enabled: bool,
        errors: std::sync::Mutex<Vec<(BranchId, Uuid, Option<String>)>>,
    }

    impl FakePort {
        fn new(run_as_key_id: Option<Uuid>, managed_key: Option<(Uuid, String)>) -> Self {
            Self {
                function_id: Uuid::new_v4(),
                database: DatabaseId(Uuid::new_v4()),
                run_as_key_id,
                managed_key,
                enabled: true,
                errors: Default::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl MetadataPort for FakePort {
        async fn resolve(
            &self,
            _branch: BranchId,
            name: &str,
        ) -> Result<Option<ResolvedFunction>, MetadataError> {
            Ok((name == "hello").then(|| ResolvedFunction {
                function_id: self.function_id,
                kind: FunctionKind::Function,
                deployment_id: Uuid::new_v4(),
                bundle_sha256: Some("a".repeat(64)),
                spec: serde_json::json!({}),
                compatibility_date: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                auth: FunctionAuth::Key,
                enabled: self.enabled,
                run_as_role: self.run_as_key_id.map(|_| "reporter".into()),
                run_as_key_id: self.run_as_key_id,
            }))
        }
        async fn branch_database(&self, _: BranchId) -> Result<Option<DatabaseId>, MetadataError> {
            Ok(Some(self.database))
        }
        async fn decrypt_secrets(
            &self,
            _: BranchId,
            _: Uuid,
        ) -> Result<Vec<(String, String)>, MetadataError> {
            Ok(vec![("API_KEY".into(), "s3cret".into())])
        }
        async fn managed_run_as_key(
            &self,
            _: BranchId,
            function: Uuid,
        ) -> Result<Option<(Uuid, String)>, MetadataError> {
            assert_eq!(function, self.function_id);
            Ok(self.managed_key.clone())
        }
        async fn set_function_last_error(
            &self,
            branch: BranchId,
            function: Uuid,
            error: Option<&str>,
        ) -> Result<(), MetadataError> {
            self.errors
                .lock()
                .unwrap()
                .push((branch, function, error.map(str::to_owned)));
            Ok(())
        }
        async fn live_database(&self, _: DatabaseId) -> Result<bool, MetadataError> {
            Ok(true)
        }
    }

    async fn service_key_of(port: FakePort) -> Option<String> {
        let dir = Metadata { client: port, public_url: "u".into() };
        let parts = dir.resolve(Uuid::new_v4(), "hello").await.unwrap().unwrap();
        parts.service_key.map(|k| k.to_string())
    }

    #[tokio::test]
    async fn a_live_managed_key_becomes_the_service_key() {
        let key = Uuid::new_v4();
        let port = FakePort::new(Some(key), Some((key, "chk_sec_reporter".into())));
        let dir = Metadata { client: port, public_url: "u".into() };
        let branch = Uuid::new_v4();
        let parts = dir.resolve(branch, "hello").await.unwrap().unwrap();
        assert_eq!(
            parts.service_key.as_deref().map(String::as_str),
            Some("chk_sec_reporter")
        );
        assert_eq!(parts.database_id, dir.client.database.as_uuid());
        let r = Resolution::build(parts, branch, "u");
        assert_eq!(env(&r)["CHELABASE_SERVICE_KEY"], "chk_sec_reporter");
    }

    #[tokio::test]
    async fn no_service_key_without_a_live_managed_key() {
        let key = Uuid::new_v4();
        // Revoked (or not managed by this function): the metadata answers no key.
        assert_eq!(service_key_of(FakePort::new(Some(key), None)).await, None);
        // No run-as role at all.
        assert_eq!(service_key_of(FakePort::new(None, None)).await, None);
        // Disabled: nothing is opened.
        let mut off = FakePort::new(Some(key), Some((key, "chk_sec_reporter".into())));
        off.enabled = false;
        assert_eq!(service_key_of(off).await, None);
    }

    #[tokio::test]
    async fn last_error_goes_through_the_metadata_call() {
        let dir = Metadata {
            client: FakePort::new(None, None),
            public_url: "u".into(),
        };
        let (branch, function) = (Uuid::new_v4(), Uuid::new_v4());
        dir.record_error(branch, function, "timed out")
            .await
            .unwrap();
        assert_eq!(
            *dir.client.errors.lock().unwrap(),
            vec![(BranchId(branch), function, Some("timed out".to_owned()))]
        );
    }

    #[test]
    fn env_holds_secrets_url_and_branch_and_the_service_key_only_when_live() {
        let branch = Uuid::new_v4();
        let r = Resolution::build(
            parts(&[("API_KEY", "s3cret")], None),
            branch,
            "https://api.example",
        );
        assert_eq!(
            env(&r),
            serde_json::json!({"API_KEY": "s3cret", "CHELABASE_URL": "https://api.example", "CHELABASE_BRANCH_ID": branch.to_string()})
        );
        let r = Resolution::build(parts(&[], Some("chk_sec_x")), branch, "u");
        assert_eq!(env(&r)["CHELABASE_SERVICE_KEY"], "chk_sec_x");
    }

    #[test]
    fn a_secret_never_overrides_the_services_own_env() {
        let branch = Uuid::new_v4();
        let r = Resolution::build(
            parts(
                &[
                    ("CHELABASE_URL", "x"),
                    ("CHELABASE_BRANCH_ID", "y"),
                    ("CHELABASE_SERVICE_KEY", "z"),
                ],
                None,
            ),
            branch,
            "u",
        );
        let env = env(&r);
        assert_eq!(env["CHELABASE_URL"], "u");
        assert_eq!(env["CHELABASE_BRANCH_ID"], branch.to_string());
        assert!(env.get("CHELABASE_SERVICE_KEY").is_none());
    }

    #[test]
    fn secrets_are_zeroizing_and_never_debug_printed() {
        // Type-level: the secret values and the env header are wiped when they drop.
        fn wiped<T: zeroize::Zeroize>(_: &Zeroizing<T>) {}
        let p = parts(&[("A", "s3cret")], Some("chk_sec_x"));
        wiped(&p.secrets[0].1);
        wiped(p.service_key.as_ref().unwrap());
        let r = Resolution::build(p, Uuid::new_v4(), "u");
        wiped(&r.env_header);
        let printed = format!("{r:?}");
        assert!(!printed.contains(r.env_header.as_str()), "{printed}");
    }
}
