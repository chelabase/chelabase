use std::{fmt, time::Duration};

pub const ENV_BACKEND: &str = "CHELABASE_STORAGE_BACKEND";
pub const ENV_ENDPOINT: &str = "CHELABASE_STORAGE_S3_ENDPOINT";
pub const ENV_REGION: &str = "CHELABASE_STORAGE_S3_REGION";
pub const ENV_BUCKET: &str = "CHELABASE_STORAGE_S3_BUCKET";
pub const ENV_PATH_STYLE: &str = "CHELABASE_STORAGE_S3_PATH_STYLE";
pub const ENV_ACCESS_KEY_FILE: &str = "CHELABASE_STORAGE_S3_ACCESS_KEY_FILE";
pub const ENV_SECRET_KEY_FILE: &str = "CHELABASE_STORAGE_S3_SECRET_KEY_FILE";
pub const ENV_CONNECT_TIMEOUT: &str = "CHELABASE_STORAGE_S3_CONNECT_TIMEOUT";
pub const ENV_REQUEST_TIMEOUT: &str = "CHELABASE_STORAGE_S3_REQUEST_TIMEOUT";
pub const ENV_MAX_ATTEMPTS: &str = "CHELABASE_STORAGE_S3_MAX_ATTEMPTS";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    SeaweedFs,
    S3,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{var} is invalid: {reason}")]
    Invalid { var: &'static str, reason: String },
    /// Never carries the file's contents.
    #[error("{var}: cannot read key file {path}: {kind}")]
    KeyFile {
        var: &'static str,
        path: String,
        kind: std::io::ErrorKind,
    },
}

#[derive(Clone)]
pub struct StorageConfig {
    pub backend: Backend,
    /// `None` means the AWS default endpoint.
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub path_style: bool,
    pub access_key: String,
    pub secret_key: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_attempts: u32,
}

// Hand-written so the keys can never reach a log.
impl fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageConfig")
            .field("backend", &self.backend)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("path_style", &self.path_style)
            .field("access_key", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_attempts", &self.max_attempts)
            .finish()
    }
}

impl StorageConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Load from any variable source; tests pass a map.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let opt = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let backend = match opt(ENV_BACKEND).as_deref() {
            None | Some("seaweedfs") => Backend::SeaweedFs,
            Some("s3") => Backend::S3,
            Some(other) => {
                return Err(ConfigError::Invalid {
                    var: ENV_BACKEND,
                    reason: format!("expected `seaweedfs` or `s3`, got {other:?}"),
                });
            }
        };
        let endpoint = opt(ENV_ENDPOINT);
        let region = opt(ENV_REGION);
        let (endpoint, region, default_path_style) = match backend {
            Backend::SeaweedFs => (
                Some(endpoint.ok_or(ConfigError::Missing(ENV_ENDPOINT))?),
                region.unwrap_or_else(|| "us-east-1".into()),
                true,
            ),
            Backend::S3 => (
                endpoint,
                region.ok_or(ConfigError::Missing(ENV_REGION))?,
                false,
            ),
        };
        let path_style = match opt(ENV_PATH_STYLE).as_deref() {
            None => default_path_style,
            Some("true") => true,
            Some("false") => false,
            Some(other) => {
                return Err(ConfigError::Invalid {
                    var: ENV_PATH_STYLE,
                    reason: format!("expected `true` or `false`, got {other:?}"),
                });
            }
        };
        let bucket = opt(ENV_BUCKET).ok_or(ConfigError::Missing(ENV_BUCKET))?;
        let duration = |var: &'static str, default: Duration| match opt(var) {
            None => Ok(default),
            Some(v) => humantime::parse_duration(&v).map_err(|e| ConfigError::Invalid {
                var,
                reason: e.to_string(),
            }),
        };
        let max_attempts =
            match opt(ENV_MAX_ATTEMPTS) {
                None => 3,
                Some(v) => v.parse::<u32>().ok().filter(|n| *n >= 1).ok_or_else(|| {
                    ConfigError::Invalid {
                        var: ENV_MAX_ATTEMPTS,
                        reason: "expected an integer of at least 1".into(),
                    }
                })?,
            };
        let read_key = |var: &'static str| {
            let path = opt(var).ok_or(ConfigError::Missing(var))?;
            let raw = std::fs::read_to_string(&path).map_err(|e| ConfigError::KeyFile {
                var,
                path: path.clone(),
                kind: e.kind(),
            })?;
            let key = raw.trim_end_matches(['\n', '\r']).to_string();
            if key.is_empty() {
                return Err(ConfigError::Invalid {
                    var,
                    reason: format!("key file {path} is empty"),
                });
            }
            Ok(key)
        };
        Ok(Self {
            backend,
            endpoint,
            region,
            bucket,
            path_style,
            access_key: read_key(ENV_ACCESS_KEY_FILE)?,
            secret_key: read_key(ENV_SECRET_KEY_FILE)?,
            connect_timeout: duration(ENV_CONNECT_TIMEOUT, Duration::from_secs(5))?,
            request_timeout: duration(ENV_REQUEST_TIMEOUT, Duration::from_secs(60))?,
            max_attempts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    struct Keys {
        _dir: tempfile::TempDir,
        access: String,
        secret: String,
    }

    fn keys(access: &str, secret: &str) -> Keys {
        let dir = tempfile::tempdir().unwrap();
        let (a, s) = (dir.path().join("access"), dir.path().join("secret"));
        std::fs::write(&a, access).unwrap();
        std::fs::write(&s, secret).unwrap();
        Keys {
            access: a.to_string_lossy().into(),
            secret: s.to_string_lossy().into(),
            _dir: dir,
        }
    }

    #[test]
    fn seaweedfs_without_endpoint_names_the_variable() {
        let k = keys("ak\n", "sk\n");
        let err = StorageConfig::from_lookup(env(&[
            (ENV_BACKEND, "seaweedfs"),
            (ENV_BUCKET, "b"),
            (ENV_ACCESS_KEY_FILE, &k.access),
            (ENV_SECRET_KEY_FILE, &k.secret),
        ]))
        .unwrap_err();
        assert!(
            err.to_string().contains("CHELABASE_STORAGE_S3_ENDPOINT"),
            "{err}"
        );
    }

    #[test]
    fn seaweedfs_defaults() {
        let k = keys("ak", "sk");
        let c = StorageConfig::from_lookup(env(&[
            (ENV_ENDPOINT, "http://seaweedfs:8333"),
            (ENV_BUCKET, "b"),
            (ENV_ACCESS_KEY_FILE, &k.access),
            (ENV_SECRET_KEY_FILE, &k.secret),
        ]))
        .unwrap();
        assert_eq!(c.backend, Backend::SeaweedFs);
        assert!(c.path_style);
        assert_eq!(c.region, "us-east-1");
        assert_eq!(c.endpoint.as_deref(), Some("http://seaweedfs:8333"));
    }

    #[test]
    fn s3_uses_aws_endpoint_and_virtual_host() {
        let k = keys("ak", "sk");
        let c = StorageConfig::from_lookup(env(&[
            (ENV_BACKEND, "s3"),
            (ENV_REGION, "eu-west-1"),
            (ENV_BUCKET, "b"),
            (ENV_ACCESS_KEY_FILE, &k.access),
            (ENV_SECRET_KEY_FILE, &k.secret),
        ]))
        .unwrap();
        assert_eq!(c.backend, Backend::S3);
        assert_eq!(c.endpoint, None);
        assert!(!c.path_style);
        assert_eq!(c.region, "eu-west-1");
    }

    #[test]
    fn s3_requires_a_region() {
        let k = keys("ak", "sk");
        let err = StorageConfig::from_lookup(env(&[
            (ENV_BACKEND, "s3"),
            (ENV_BUCKET, "b"),
            (ENV_ACCESS_KEY_FILE, &k.access),
            (ENV_SECRET_KEY_FILE, &k.secret),
        ]))
        .unwrap_err();
        assert!(
            err.to_string().contains("CHELABASE_STORAGE_S3_REGION"),
            "{err}"
        );
    }

    #[test]
    fn overrides_win() {
        let k = keys("ak", "sk");
        let c = StorageConfig::from_lookup(env(&[
            (ENV_BACKEND, "s3"),
            (ENV_REGION, "r"),
            (ENV_ENDPOINT, "https://minio.example"),
            (ENV_PATH_STYLE, "true"),
            (ENV_BUCKET, "b"),
            (ENV_CONNECT_TIMEOUT, "3s"),
            (ENV_REQUEST_TIMEOUT, "2m"),
            (ENV_MAX_ATTEMPTS, "5"),
            (ENV_ACCESS_KEY_FILE, &k.access),
            (ENV_SECRET_KEY_FILE, &k.secret),
        ]))
        .unwrap();
        assert!(c.path_style);
        assert_eq!(c.endpoint.as_deref(), Some("https://minio.example"));
        assert_eq!(c.connect_timeout, Duration::from_secs(3));
        assert_eq!(c.request_timeout, Duration::from_secs(120));
        assert_eq!(c.max_attempts, 5);
    }

    #[test]
    fn unknown_backend_is_rejected() {
        let err = StorageConfig::from_lookup(env(&[(ENV_BACKEND, "gcs")])).unwrap_err();
        assert!(err.to_string().contains(ENV_BACKEND), "{err}");
    }

    #[test]
    fn key_file_trailing_newline_is_trimmed() {
        let k = keys("access-id\n", "secret-val\r\n");
        let c = StorageConfig::from_lookup(env(&[
            (ENV_ENDPOINT, "http://x"),
            (ENV_BUCKET, "b"),
            (ENV_ACCESS_KEY_FILE, &k.access),
            (ENV_SECRET_KEY_FILE, &k.secret),
        ]))
        .unwrap();
        assert_eq!(c.access_key, "access-id");
        assert_eq!(c.secret_key, "secret-val");
    }

    #[test]
    fn missing_key_file_fails_without_printing_a_key() {
        let k = keys("TOPSECRET-ACCESS", "TOPSECRET-SECRET");
        let missing = format!("{}/nope", k.access);
        let err = StorageConfig::from_lookup(env(&[
            (ENV_ENDPOINT, "http://x"),
            (ENV_BUCKET, "b"),
            (ENV_ACCESS_KEY_FILE, &missing),
            (ENV_SECRET_KEY_FILE, &k.secret),
        ]))
        .unwrap_err();
        let msg = format!("{err} {err:?}");
        assert!(msg.contains(ENV_ACCESS_KEY_FILE), "{msg}");
        assert!(!msg.contains("TOPSECRET"), "{msg}");
    }

    #[test]
    fn debug_never_shows_keys() {
        let k = keys("TOPSECRET-ACCESS", "TOPSECRET-SECRET");
        let c = StorageConfig::from_lookup(env(&[
            (ENV_ENDPOINT, "http://x"),
            (ENV_BUCKET, "b"),
            (ENV_ACCESS_KEY_FILE, &k.access),
            (ENV_SECRET_KEY_FILE, &k.secret),
        ]))
        .unwrap();
        assert!(!format!("{c:?}").contains("TOPSECRET"));
    }
}
