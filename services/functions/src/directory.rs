//! Where a function's serving state comes from: the metadata database, behind [`Directory`] so
//! the service's tests use a fake.

use std::collections::BTreeMap;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chelabase_metadata::MetadataClient;
use chelabase_metadata::{FunctionAuth, FunctionKind};
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
}

/// The metadata database. Its client carries the secrets key (`with_secrets_key`): function
/// secrets and the managed run-as key open with it.
pub struct Metadata {
    pub client: MetadataClient,
}

#[async_trait::async_trait]
impl Directory for Metadata {
    async fn resolve(&self, branch: Uuid, name: &str) -> anyhow::Result<Option<Parts>> {
        let Some(f) = self.client.resolve(BranchId(branch), name).await? else {
            return Ok(None);
        };
        let (FunctionKind::Function, Some(sha)) = (f.kind, f.bundle_sha256) else {
            return Ok(None);
        };
        let Some(record) = self.client.get_branch(BranchId(branch)).await? else {
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
            database_id: record.database_id.as_uuid(),
            function_id: f.function_id,
            bundle_sha256: sha,
            auth: f.auth,
            enabled: f.enabled,
            secrets,
            service_key,
        }))
    }

    async fn record_error(&self, branch: Uuid, function: Uuid, error: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE branch_functions SET last_error = $3 WHERE branch_id = $1 AND function_id = $2",
        )
        .bind(branch)
        .bind(function)
        .bind(error)
        .execute(self.client.pool())
        .await?;
        Ok(())
    }

    async fn database_live(&self, database: Uuid) -> anyhow::Result<bool> {
        Ok(self.client.live_database(DatabaseId(database)).await?)
    }
}

impl Metadata {
    /// The function's live managed key on the branch, opened (`run_as_role`'s key).
    async fn service_key(
        &self,
        branch: Uuid,
        function: Uuid,
    ) -> anyhow::Result<Option<Zeroizing<String>>> {
        // TODO(#180): `self.client.managed_run_as_key(BranchId(branch), function)` returns the
        // live, unrevoked key managed by this function, opened with the client's secrets key.
        // Until #180 lands no function gets CHELABASE_SERVICE_KEY.
        let _ = (branch, function);
        Ok(None)
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
        }
    }

    fn env(r: &Resolution) -> serde_json::Value {
        serde_json::from_slice(&STANDARD.decode(r.env_header.as_bytes()).unwrap()).unwrap()
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
