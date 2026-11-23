//! Which API operation a sandbox's `chelabase.request` names: the path is checked for shape,
//! then matched against the spec bundle's path templates. Only an operation in the bundle can
//! be called, so an excluded one (not in the bundle) never matches.
//!
//! **Invariant:** what this matcher accepts is what the gateway routes. Both match the raw
//! path (no decoding) segment by segment; the candidates are the templates whose method
//! matches, and among those the one whose first differing segment is a literal wins
//! (`chelabase_routes::match_endpoint`). A path accepted here is in canonical form, so neither
//! the URL the host builds nor the gateway can read it differently: only RFC 3986 path
//! characters, no empty, `.` or `..` segment (raw or percent-encoded), no encoded `/` or `\`,
//! no query or fragment. The property test in this module checks every accepted path against
//! the gateway's own matcher.
//!
//! A parameter matches one non-empty segment. The API's storage object routes take a
//! multi-segment `{*name}`; here a nested object name doesn't match and is refused (fail
//! closed).

use std::sync::LazyLock;

use serde_json::Value;

/// One path segment of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Param(String),
}

#[derive(Debug)]
struct Operation {
    method: String,
    template: String,
    segments: Vec<Segment>,
    operation_id: String,
}

/// The bundle's operations, ready to match.
#[derive(Debug)]
pub struct Matcher {
    operations: Vec<Operation>,
}

/// A matched call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched<'m, 'p> {
    pub operation_id: &'m str,
    /// The path template, as the bundle writes it.
    pub template: &'m str,
    /// Each parameter's name and raw segment, in order.
    pub params: Vec<(&'m str, &'p str)>,
}

/// Why a path was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Not a canonical absolute path.
    Shape,
    /// On the deny list, whatever the bundle holds.
    Denied,
    /// No bundle operation has this method and path.
    NoOperation,
}

/// Path prefixes refused whatever the bundle holds: the MCP's own account routes.
pub const DENIED_PREFIXES: &[&str] = &["/v1/me/mcp-"];

static BUNDLE_MATCHER: LazyLock<Matcher> = LazyLock::new(|| {
    let bundle: Value = serde_json::from_str(crate::tools::BUNDLE).expect("openapi/mcp.json");
    Matcher::from_bundle(&bundle).expect("the bundle's path templates are well-formed")
});

impl Matcher {
    /// The matcher for the committed bundle (`openapi/mcp.json`).
    pub fn bundle() -> &'static Matcher {
        &BUNDLE_MATCHER
    }

    /// A matcher over `bundle`'s `paths`. Fails on a template it can't match exactly: one that
    /// isn't canonical, or has a `{*tail}`, or a segment mixing a parameter with text.
    pub fn from_bundle(bundle: &Value) -> anyhow::Result<Self> {
        let paths = bundle["paths"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("the bundle has no paths"))?;
        let mut operations = Vec::new();
        for (template, ops) in paths {
            let raw = split(template, true)
                .map_err(|_| anyhow::anyhow!("{template:?} is not a canonical path"))?;
            let template_segments = raw
                .iter()
                .map(
                    |s| match s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                        Some(name) if is_param_name(name) => Ok(Segment::Param(name.to_owned())),
                        Some(_) => anyhow::bail!("{template:?}: a parameter must be {{name}}"),
                        None if s.contains(['{', '}']) => {
                            anyhow::bail!("{template:?}: a parameter must be a whole segment")
                        }
                        None => Ok(Segment::Literal((*s).to_owned())),
                    },
                )
                .collect::<anyhow::Result<Vec<_>>>()?;
            let ops = ops
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("{template:?} is not an object"))?;
            for (method, op) in ops {
                let operation_id = op["operationId"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("{method} {template} has no operationId"))?;
                operations.push(Operation {
                    method: method.to_ascii_uppercase(),
                    template: template.clone(),
                    segments: template_segments.clone(),
                    operation_id: operation_id.to_owned(),
                });
            }
        }
        Ok(Self { operations })
    }

    /// The operation `method` (upper case) on `path` names.
    pub fn find<'m, 'p>(&'m self, method: &str, path: &'p str) -> Result<Matched<'m, 'p>, Refused> {
        let raw = segments(path)?;
        if denied(&raw) {
            return Err(Refused::Denied);
        }
        // The gateway's rule: among the templates with this method that match, the one whose
        // first differing segment is a literal. Two equally specific matches refuse.
        let mut best: Option<(&Operation, Vec<bool>)> = None;
        let mut tied = false;
        for op in self.operations.iter().filter(|o| o.method == method) {
            let Some(specificity) = specificity(&op.segments, &raw) else {
                continue;
            };
            match &best {
                Some((_, current)) if specificity == *current => tied = true,
                Some((_, current)) if specificity < *current => {}
                _ => {
                    best = Some((op, specificity));
                    tied = false;
                }
            }
        }
        let (op, _) = best.ok_or(Refused::NoOperation)?;
        if tied {
            return Err(Refused::NoOperation);
        }
        let params = op
            .segments
            .iter()
            .zip(&raw)
            .filter_map(|(t, p)| match t {
                Segment::Param(name) => Some((name.as_str(), *p)),
                Segment::Literal(_) => None,
            })
            .collect();
        Ok(Matched {
            operation_id: &op.operation_id,
            template: &op.template,
            params,
        })
    }

    /// Every `(METHOD, template, operationId)`.
    pub fn operations(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.operations.iter().map(|o| {
            (
                o.method.as_str(),
                o.template.as_str(),
                o.operation_id.as_str(),
            )
        })
    }
}

/// `path`'s segments when it is canonical (see the module docs).
pub fn segments(path: &str) -> Result<Vec<&str>, Refused> {
    split(path, false)
}

/// [`segments`], letting `{` and `}` through when `template` (a template's own braces).
fn split(path: &str, template: bool) -> Result<Vec<&str>, Refused> {
    let rest = path.strip_prefix('/').ok_or(Refused::Shape)?;
    let segments: Vec<&str> = rest.split('/').collect();
    for segment in &segments {
        if !canonical_segment(segment, template) {
            return Err(Refused::Shape);
        }
    }
    Ok(segments)
}

/// A non-empty segment of RFC 3986 `pchar`s whose percent-decoded form is UTF-8 without `/`,
/// `\`, a control character, and is not `.` or `..`.
fn canonical_segment(segment: &str, template: bool) -> bool {
    if segment.is_empty() {
        return false;
    }
    let pchar = |b: u8| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
            )
            || (template && matches!(b, b'{' | b'}'))
    };
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok());
                let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) else {
                    return false;
                };
                decoded.push(byte);
                i += 3;
            }
            b if pchar(b) => {
                decoded.push(b);
                i += 1;
            }
            _ => return false,
        }
    }
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    decoded != "."
        && decoded != ".."
        && !decoded
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

fn is_param_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether `raw` (a path's segments) falls under [`DENIED_PREFIXES`], compared on the decoded
/// path.
fn denied(raw: &[&str]) -> bool {
    let decoded: String = raw
        .iter()
        .map(|s| format!("/{}", percent_decode(s)))
        .collect();
    DENIED_PREFIXES.iter().any(|p| decoded.starts_with(p))
}

fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match byte {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One flag per segment (`true` for a literal) when `template` matches `raw`.
fn specificity(template: &[Segment], raw: &[&str]) -> Option<Vec<bool>> {
    if template.len() != raw.len() {
        return None;
    }
    template
        .iter()
        .zip(raw)
        .map(|(t, p)| match t {
            Segment::Literal(l) => (l == p).then_some(true),
            Segment::Param(_) => Some(false),
        })
        .collect()
}

/// `template` with each `{param}` replaced by its value in `params`.
pub fn render(template: &str, params: &[(&str, &str)]) -> String {
    template
        .split('/')
        .map(|s| {
            s.strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .and_then(|name| params.iter().find(|(n, _)| *n == name))
                .map_or(s, |(_, v)| *v)
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const BRANCH: &str = "/v1/databases/db_1/branches/br_2";

    fn find(method: &str, path: &str) -> Result<&'static str, Refused> {
        Matcher::bundle().find(method, path).map(|m| m.operation_id)
    }

    #[test]
    fn matches_a_template() {
        assert_eq!(find("GET", "/v1/databases/db_1"), Ok("getDatabase"));
        assert_eq!(find("DELETE", "/v1/databases/db_1"), Ok("deleteDatabase"));
        assert_eq!(find("POST", &format!("{BRANCH}/sql")), Ok("runSql"));
        assert_eq!(
            find("POST", &format!("{BRANCH}/console/query")),
            Ok("consoleQuery")
        );
        let m = Matcher::bundle()
            .find("GET", "/v1/databases/db_1/merges/m_9")
            .unwrap();
        assert_eq!(m.template, "/v1/databases/{id}/merges/{merge_id}");
        assert_eq!(m.params, vec![("id", "db_1"), ("merge_id", "m_9")]);
        // The literal wins over a parameter in the same place (as the gateway routes it).
        assert_eq!(
            find("POST", &format!("{BRANCH}/storage/object/list/avatars")),
            Ok("storageListObjects")
        );
        assert_eq!(
            find("POST", &format!("{BRANCH}/storage/object/copy")),
            Ok("storageCopyObject")
        );
        // Wrong method, unknown path, excluded operation: no match.
        assert_eq!(find("PUT", "/v1/databases/db_1"), Err(Refused::NoOperation));
        assert_eq!(
            find("HEAD", "/v1/databases/db_1"),
            Err(Refused::NoOperation)
        );
        assert_eq!(find("get", "/v1/databases/db_1"), Err(Refused::NoOperation));
        assert_eq!(find("GET", "/v1/nope"), Err(Refused::NoOperation));
        assert_eq!(
            find("POST", &format!("{BRANCH}/functions/f/deployments")),
            Err(Refused::NoOperation),
            "createFunctionDeployment is excluded"
        );
        assert_eq!(
            find("PUT", &format!("{BRANCH}/storage/object/b/a.png")),
            Err(Refused::NoOperation),
            "storagePutObject is excluded"
        );
        for path in [
            "/auth/v1/token",
            "/mcp",
            "/internal/spec",
            "/.well-known/oauth-protected-resource",
            "/data/v1/query",
        ] {
            for method in ["GET", "POST"] {
                assert_eq!(find(method, path), Err(Refused::NoOperation), "{path}");
            }
        }
    }

    #[test]
    fn refuses_dot_segments() {
        for path in [
            "/v1/databases/..",
            "/v1/databases/.",
            "/v1/databases/db_1/../db_2",
            "/v1/./databases/db_1",
            "/v1/databases/%2e%2e",
            "/v1/databases/%2E",
            "/v1/databases/.%2e",
        ] {
            assert_eq!(find("GET", path), Err(Refused::Shape), "{path}");
        }
        // A dot inside a segment is fine.
        assert_eq!(find("GET", "/v1/databases/a.b"), Ok("getDatabase"));
    }

    #[test]
    fn refuses_encoded_slash() {
        for path in [
            "/v1/databases/a%2Fb",
            "/v1/databases/a%2fb",
            "/v1/databases/a%5Cb",
            "/v1/databases/a%5cb",
            "/v1/databases/a\\b",
            "/v1/databases/a%00b",
            "/v1/databases/a%zzb",
            "/v1/databases/a%",
            "/v1/databases/a%ffb",
            "/v1/databases//",
            "/v1/databases/",
            "/v1//databases",
            "/v1/databases/a\u{0}b",
            "/v1/databases/a\nb",
            "/v1/databases/a\u{7f}b",
            "/v1/databases/a b",
            "/v1/databases/a\u{e9}b",
        ] {
            assert_eq!(find("GET", path), Err(Refused::Shape), "{path:?}");
        }
        // Other escapes are a parameter's own bytes.
        assert_eq!(find("GET", "/v1/databases/a%20b"), Ok("getDatabase"));
    }

    #[test]
    fn refuses_a_query_in_the_path() {
        for path in [
            "/v1/databases/db_1?x=1",
            "/v1/databases/db_1?",
            "/v1/databases/db_1#frag",
        ] {
            assert_ne!(find("GET", path), Ok("getDatabase"), "{path}");
        }
        assert_eq!(find("GET", "/v1/databases/db_1?x=1"), Err(Refused::Shape));
        assert_eq!(find("GET", "/v1/databases/db_1#frag"), Err(Refused::Shape));
        // An escaped `?` is a parameter's own byte, not a query.
        assert_eq!(find("GET", "/v1/databases/db_1%3Fx"), Ok("getDatabase"));
    }

    #[test]
    fn refuses_absolute_urls() {
        for path in [
            "https://evil.example/v1/databases",
            "http://api.example.test/v1/databases/db_1",
            "//evil.example/v1/databases/db_1",
            "v1/databases/db_1",
            "",
            "/",
            "https:/v1/databases",
        ] {
            assert_eq!(find("GET", path), Err(Refused::Shape), "{path}");
        }
    }

    #[test]
    fn a_parameter_is_one_segment() {
        assert_eq!(find("GET", "/v1/databases/a/b"), Err(Refused::NoOperation));
        assert_eq!(
            find("DELETE", &format!("{BRANCH}/storage/object/b/u1/a.png")),
            Err(Refused::NoOperation),
            "a nested object name is refused"
        );
        assert_eq!(
            find("DELETE", &format!("{BRANCH}/storage/object/b/a.png")),
            Ok("storageDeleteObject")
        );
    }

    #[test]
    fn the_mcp_account_routes_are_denied() {
        for path in ["/v1/me/mcp-grants", "/v1/me/mcp-grants/g_1", "/v1/me/mcp-"] {
            for method in ["GET", "POST", "DELETE"] {
                assert_eq!(find(method, path), Err(Refused::Denied), "{path}");
            }
        }
        // Decoded too.
        assert_eq!(find("GET", "/v1/me/mcp%2Dgrants"), Err(Refused::Denied));
    }

    #[test]
    fn templates_the_matcher_cannot_match_exactly_are_refused_at_build() {
        for template in ["/v1/a/{*rest}", "/v1/a/x{id}", "/v1/a/", "v1/a", "/v1/a/{}"] {
            let bundle = serde_json::json!({
                "paths": {template: {"get": {"operationId": "op"}}}
            });
            assert!(Matcher::from_bundle(&bundle).is_err(), "{template}");
        }
        let bundle = serde_json::json!({"paths": {"/v1/a": {"get": {}}}});
        assert!(
            Matcher::from_bundle(&bundle).is_err(),
            "an operation needs an id"
        );
    }

    #[test]
    fn render_fills_parameters() {
        assert_eq!(
            render(
                "/v1/databases/{id}/merges/{merge_id}",
                &[("id", "a"), ("merge_id", "b")]
            ),
            "/v1/databases/a/merges/b"
        );
    }

    /// Every word a gateway route template uses, so generated parameters can collide with
    /// literal segments (where a wrong match would show).
    fn words() -> Vec<String> {
        let mut words: Vec<String> = chelabase_routes::ENDPOINTS
            .iter()
            .flat_map(|e| e.path.split('/'))
            .filter(|s| !s.is_empty() && !s.starts_with('{'))
            .map(str::to_owned)
            .collect();
        words.extend(["x", "a.b", "a%20b", "mcp-grants", "_"].map(str::to_owned));
        words.sort();
        words.dedup();
        words
    }

    fn gateway_template(path: &str) -> String {
        path.replace("{*", "{")
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        /// Any path accepted maps to exactly one bundle operation, re-renders to the same path,
        /// and is the operation the gateway routes it to.
        #[test]
        fn an_accepted_path_is_one_operation_and_what_the_gateway_routes(
            op in 0usize..1000,
            picks in proptest::collection::vec(0usize..10_000, 8),
            method_pick in 0usize..6,
        ) {
            let matcher = Matcher::bundle();
            let ops: Vec<_> = matcher.operations().collect();
            let (op_method, template, _) = ops[op % ops.len()];
            let words = words();
            let mut i = 0;
            let path: String = template
                .split('/')
                .map(|s| {
                    if s.starts_with('{') {
                        i += 1;
                        words[picks[i - 1] % words.len()].clone()
                    } else {
                        s.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            let method = ["GET", "POST", "PUT", "PATCH", "DELETE", op_method][method_pick];
            let Ok(m) = matcher.find(method, &path) else {
                return Ok(());
            };
            // Exactly one operation has this method and template.
            let same: Vec<_> = ops
                .iter()
                .filter(|(om, t, _)| *om == method && *t == m.template)
                .collect();
            prop_assert_eq!(same.len(), 1);
            prop_assert_eq!(same[0].2, m.operation_id);
            prop_assert_eq!(render(m.template, &m.params), path.clone());
            // The gateway routes it to the same operation.
            match chelabase_routes::match_endpoint(chelabase_routes::Scope::Api, method, &path) {
                chelabase_routes::Match::Found(ep) => {
                    prop_assert_eq!(gateway_template(ep.path), m.template.to_owned());
                    prop_assert_eq!(ep.method.as_str(), method);
                }
                other => prop_assert!(false, "{method} {path}: the gateway says {other:?}"),
            }
        }
    }

    /// Every bundle operation is a gateway route with the same method and template.
    #[test]
    fn every_bundle_operation_is_a_gateway_route() {
        let routes: Vec<(String, &str)> = chelabase_routes::ENDPOINTS
            .iter()
            .map(|e| (gateway_template(e.path), e.method.as_str()))
            .collect();
        for (method, template, id) in Matcher::bundle().operations() {
            assert!(
                routes.iter().any(|(t, m)| t == template && *m == method),
                "{id}: {method} {template} is not a gateway route"
            );
        }
    }
}
