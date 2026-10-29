//! `chelabase gen types`: fetches the data API's schema and writes `@chelabase/client`'s
//! TypeScript module. The generator itself is [`chelabase_typegen`] (which `rest` serves as
//! `GET /data/v1/types` too); this is the fetch and the file.

use anyhow::{Context, Result, bail};
use chelabase_typegen::{Format, Schema, quote};
use chelabase_types::data_key::{DataKeyKind, parse_kind};
use std::path::Path;

pub use chelabase_typegen::Which;

/// How long the schema call may take.
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `chelabase gen types`: checks the key, fetches the schema (or schemas), writes the module.
pub async fn run(url: &str, key: Option<String>, out: Option<&Path>, which: Which) -> Result<()> {
    fetch_and_write(url, key, out, which, FETCH_TIMEOUT).await
}

async fn fetch_schema(
    http: &chelabase_http::Client,
    url: &str,
    key: &str,
    database: Option<&str>,
) -> Result<Schema> {
    let endpoint = format!("{}/data/v1/schema", url.trim_end_matches('/'));
    let mut target = reqwest::Url::parse(&endpoint).with_context(|| format!("the URL {endpoint}"))?;
    if let Some(d) = database {
        target.query_pairs_mut().append_pair("database", d);
    }
    let resp = http
        .get(target)
        .bearer_auth(key)
        .send().await.with_context(|| format!("GET {endpoint}"))?;
    let status = resp.status();
    let body = resp.text().await.context("reading the schema response")?;
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        let which = database.map(|d| format!(" (database {})", quote(d))).unwrap_or_default();
        bail!("GET {endpoint}{which} answered {status}: {snippet}");
    }
    serde_json::from_str(&body)
        .context("the schema response isn't the data API schema (is the server up to date? it must name `default_schema`)")
}

async fn fetch_and_write(
    url: &str,
    key: Option<String>,
    out: Option<&Path>,
    which: Which,
    timeout: std::time::Duration,
) -> Result<()> {
    let key =
        key.context("no data key: pass --key or set CHELABASE_DATA_KEY (a secret key, chk_sec_…)")?;
    if parse_kind(key.trim()) != Some(DataKeyKind::Secret) {
        bail!(
            "gen types needs a secret key (chk_sec_…); a public key, or anything that isn't a data key, can't read the schema"
        );
    }
    let key = key.trim();
    // Never a redirect (#26): the secret data key goes to the API host only.
    let http = chelabase_http::client_builder()
        .timeout(timeout)
        .build()
        .context("an HTTP client")?;
    let http = &http;
    let schemas = chelabase_typegen::collect(&which, |database: Option<String>| async move {
        fetch_schema(http, url, key, database.as_deref()).await
    })
    .await?;
    let text = chelabase_typegen::render(&schemas, Format::Ts)?;
    match out {
        Some(path) => {
            std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?
        }
        None => print!("{text}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Hits = Arc<Mutex<Vec<(String, String)>>>;

    const SCHEMA: &str = include_str!("../../../../packages/client/fixtures/gen/schema.json");
    const GOLDEN: &str = include_str!("../../../../packages/client/fixtures/gen/db.ts");

    /// The schema call gives up after its timeout.
    #[tokio::test]
    async fn the_schema_call_times_out() {
        assert_eq!(FETCH_TIMEOUT, std::time::Duration::from_secs(30));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        // Accepts and never answers.
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                open.push(socket);
            }
        });
        let key =
            chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret).key;
        let started = std::time::Instant::now();
        let err = fetch_and_write(&url, Some(key), None, Which::Main, std::time::Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{err:#}"
        );
        held.abort();
    }

    #[tokio::test]
    async fn refuses_public_key() {
        let hits = Hits::default();
        let url = serve(hits.clone()).await;
        let public =
            chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Public).key;
        for bad in [public.as_str(), "nonsense"] {
            let err = run(&url, Some(bad.into()), None, Which::Main)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("secret key"), "{err}");
        }
        let err = run(&url, None, None, Which::Main).await.unwrap_err().to_string();
        assert!(err.contains("CHELABASE_DATA_KEY"), "{err}");
        assert!(
            hits.lock().unwrap().is_empty(),
            "no call before the key is checked"
        );
    }

    #[tokio::test]
    async fn fetches_with_the_secret_key_and_writes_the_file() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let url = serve(hits.clone()).await;
        let key =
            chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret).key;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.ts");
        run(&format!("{url}/"), Some(key.clone()), Some(&out), Which::Main)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), GOLDEN);
        let hits = hits.lock().unwrap();
        assert_eq!(
            hits.as_slice(),
            [("/data/v1/schema".to_string(), format!("Bearer {key}"))]
        );
    }

    #[tokio::test]
    async fn a_refused_call_is_an_error_naming_the_status() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let url = serve_status(hits, 401).await;
        let key =
            chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret).key;
        let err = run(&url, Some(key), None, Which::Main).await.unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
    }

    const ANALYTICS: &str =
        include_str!("../../../../packages/client/fixtures/gen/analytics.schema.json");
    const MULTI_GOLDEN: &str = include_str!("../../../../packages/client/fixtures/gen/multi.ts");

    /// Serves the main schema (with a `databases` list) or the analytics one by `?database=`,
    /// recording `path?query` and the bearer.
    async fn serve_multi(hits: Hits) -> String {
        use axum::{Router, extract::State, http::HeaderMap, http::StatusCode, http::Uri, routing::get};
        let app = Router::new()
            .fallback(get(
                move |State(h): State<Hits>, uri: Uri, headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let pq = uri.path_and_query().unwrap().to_string();
                    h.lock().unwrap().push((pq.clone(), auth.to_string()));
                    match uri.query() {
                        None => (StatusCode::OK, SCHEMA.replacen("{", "{\"databases\":[\"app_main\",\"analytics\"],", 1)),
                        Some("database=analytics") => (StatusCode::OK, ANALYTICS.to_string()),
                        Some(_) => (StatusCode::NOT_FOUND, "{}".to_string()),
                    }
                },
            ))
            .with_state(hits);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn secret() -> String {
        chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret).key
    }

    #[tokio::test]
    async fn database_flag_adds_the_query_string() {
        let hits = Hits::default();
        let url = serve_multi(hits.clone()).await;
        let key = secret();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.ts");
        run(&url, Some(key.clone()), Some(&out), Which::One("analytics".into()))
            .await
            .unwrap();
        assert_eq!(
            hits.lock().unwrap().as_slice(),
            [("/data/v1/schema?database=analytics".to_string(), format!("Bearer {key}"))]
        );
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("export type Database = {") && text.contains("reports: {"), "{text}");
        assert!(!text.contains("Databases"), "{text}");
    }

    #[tokio::test]
    async fn all_databases_reads_the_list_then_fetches_each() {
        let hits = Hits::default();
        let url = serve_multi(hits.clone()).await;
        let key = secret();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.ts");
        run(&url, Some(key.clone()), Some(&out), Which::All).await.unwrap();
        let paths: Vec<String> = hits.lock().unwrap().iter().map(|h| h.0.clone()).collect();
        assert_eq!(paths, ["/data/v1/schema", "/data/v1/schema?database=analytics"]);
        // Main's real name (`app_main`) is skipped, not fetched twice; it keeps the key "main".
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(!text.contains("app_main"), "{text}");
        assert_eq!(text, MULTI_GOLDEN);
    }

    #[tokio::test]
    async fn all_databases_with_only_main_writes_the_plain_file() {
        // The default mock has no `databases` field: one database, today's output.
        let hits = Hits::default();
        let url = serve(hits.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.ts");
        run(&url, Some(secret()), Some(&out), Which::All).await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), GOLDEN);
        assert_eq!(hits.lock().unwrap().len(), 1);
    }

    async fn serve(hits: Hits) -> String {
        serve_status(hits, 200).await
    }

    async fn serve_status(hits: Hits, status: u16) -> String {
        use axum::{
            Router, extract::State, http::HeaderMap, http::StatusCode, http::Uri, routing::get,
        };
        let app = Router::new()
            .fallback(get(
                move |State(h): State<Hits>, uri: Uri, headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    h.lock()
                        .unwrap()
                        .push((uri.path().to_string(), auth.to_string()));
                    (StatusCode::from_u16(status).unwrap(), SCHEMA)
                },
            ))
            .with_state(hits);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }
}
