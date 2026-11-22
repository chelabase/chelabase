//! The operation table against the management API: every operation has exactly one class, the
//! table names nothing the API lacks, and the plan's named lists hold.

// In a module so `cargo nextest run -p mcp policy` selects these tests by name.
mod policy {
    use mcp::policy::{self as table, Class};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn management() -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../openapi/management.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        serde_json::from_str(&text).unwrap()
    }

    /// `(operationId, METHOD)` for every operation of `doc`.
    fn operations(doc: &Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for item in doc["paths"].as_object().unwrap().values() {
            for (method, op) in item.as_object().unwrap() {
                if let Some(id) = op.get("operationId").and_then(Value::as_str) {
                    out.push((id.to_string(), method.to_ascii_uppercase()));
                }
            }
        }
        out
    }

    fn class_of(id: &str) -> Class {
        let (_, method, class) = table::TABLE
            .iter()
            .find(|(op, _, _)| *op == id)
            .unwrap_or_else(|| panic!("{id} is not in the table"));
        assert_eq!(table::static_class(id, method), Some(*class));
        *class
    }

    #[test]
    fn every_operation_is_classified() {
        let doc = management();
        let coverage = table::coverage(&doc);
        assert!(
            coverage.missing.is_empty(),
            "operations without a class: {:?}",
            coverage.missing
        );
        assert!(
            coverage.duplicate.is_empty(),
            "classified twice: {:?}",
            coverage.duplicate
        );
        for (id, method) in operations(&doc) {
            assert!(table::static_class(&id, &method).is_some(), "{id}");
        }
        assert_eq!(operations(&doc).len(), table::TABLE.len());
    }

    #[test]
    fn no_unknown_operation_in_the_table() {
        let doc = management();
        let coverage = table::coverage(&doc);
        assert!(
            coverage.unknown.is_empty(),
            "the table names operations the API lacks: {:?}",
            coverage.unknown
        );

        // Failure path: a document with one more operation names it as missing.
        let mut extra = doc.clone();
        extra["paths"]["/v1/new-thing"] =
            json!({"post": {"operationId": "createNewThing", "responses": {}}});
        let coverage = table::coverage(&extra);
        assert_eq!(coverage.missing.len(), 1, "{:?}", coverage.missing);
        assert!(coverage.missing[0].contains("createNewThing"));

        // An operation without an operationId cannot be classified either.
        let mut anonymous = doc.clone();
        anonymous["paths"]["/v1/anonymous"] = json!({"delete": {"responses": {}}});
        assert_eq!(table::coverage(&anonymous).missing.len(), 1);

        // A known name under another method is missing, and its table entry unknown.
        let mut moved = doc.clone();
        let me = moved["paths"]["/v1/me"]["get"].take();
        moved["paths"]["/v1/me"] = json!({"post": me});
        let coverage = table::coverage(&moved);
        assert!(coverage.missing.iter().any(|m| m.contains("getMe")));
        assert!(coverage.unknown.iter().any(|m| m.contains("getMe")));

        // A document missing an operation the table names reports it as unknown.
        let mut fewer = doc;
        fewer["paths"].as_object_mut().unwrap().remove("/v1/me");
        let coverage = table::coverage(&fewer);
        assert!(coverage.missing.is_empty());
        assert_eq!(coverage.unknown.len(), 1, "{:?}", coverage.unknown);
        assert!(coverage.unknown[0].contains("getMe"));

        // An operation the table lacks is never callable.
        assert_eq!(
            table::classify("createNewThing", "POST", None),
            Class::Excluded
        );
        assert_eq!(
            table::classify("createNewThing", "POST", Some(&json!({}))),
            Class::Excluded
        );
        assert_eq!(table::classify("getMe", "DELETE", None), Class::Excluded);
    }

    #[test]
    fn deletes_are_never_read_or_write() {
        for (id, method) in operations(&management()) {
            if method == "DELETE" {
                let class = table::static_class(&id, &method).unwrap();
                assert!(
                    matches!(class, Class::Destructive | Class::Excluded),
                    "{id} is {class:?}"
                );
                for body in [json!({}), json!({"read_only": true, "dry_run": true})] {
                    let class = table::classify(&id, &method, Some(&body));
                    assert!(
                        matches!(class, Class::Destructive | Class::Excluded),
                        "{id} is {class:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn gets_are_never_write() {
        for (id, method) in operations(&management()) {
            if method == "GET" {
                let class = table::static_class(&id, &method).unwrap();
                assert!(
                    matches!(class, Class::Read | Class::Excluded),
                    "{id} is {class:?}"
                );
            }
        }
    }

    /// The plan's destructive list, written out once.
    const DESTRUCTIVE: &[&str] = &[
        "resetBranchToParent",
        "restoreBranch",
        "applyMerge",
        "finishMerge",
        "retryMerge",
        "abandonMerge",
        "cancelMerge",
        "createRole",
        "updateRole",
        "putGuardExemption",
        "revokeMemberPostgresAccess",
        "putProjectGrant",
        "putGrantOverride",
        "putTeamGrant",
        "putTeamMember",
        "updateMember",
        "suspendMember",
        "reinstateMember",
        "approveMember",
        "declineMember",
        "updateTenant",
        "updateInstanceSettings",
        "updateKey",
        "enableDataApi",
        "putAppUsers",
        "deactivateAppUser",
        "reactivateAppUser",
        "updateAppUser",
        "updateBranch",
        "updatePgDatabase",
        "updateDatabase",
        "putFunction",
        "updateFunction",
        "fixBranchReviewItem",
        "acknowledgeBranchReview",
        "storageUpdateBucket",
        "updateStorageWebhook",
        "testStorageWebhook",
        // Credentials: each creates, replaces or invalidates one, or opens an outbound target.
        "createKey",
        "rotateKey",
        "createPersonalLogin",
        "resetPersonalLoginPassword",
        "resetBranchPassword",
        "resetDatabasePassword",
        "resetRolePassword",
        "createStorageWebhook",
        "rotateStorageWebhookSecret",
        // Invites carry grants; a resend mints a new link and voids the old one.
        "createInvite",
        "createProjectInvite",
        "resendInvite",
        // One PUT creates or overwrites a secret whose value never comes back.
        "putFunctionSecret",
        // Each makes deployed code run on a schedule or on every matching event.
        "createFunctionSchedule",
        "updateFunctionSchedule",
        "createFunctionTrigger",
        // Builder's calls beyond the plan: a copy or move can put an object in a public bucket
        // (and a move removes its old name), a trigger turned off drops its queued events, and a
        // new app user is a new principal with a password.
        "storageCopyObject",
        "storageMoveObject",
        "updateFunctionTrigger",
        "createAppUser",
        // Body-dependent routes: the class of a call whose body can't be read.
        "consoleQuery",
        "runSql",
        "storageCreateBucket",
        "applySchemaChanges",
    ];

    /// The binary routes. The MCP's own account routes (epics `mcp-oauth`, `mcp-sealed`) join
    /// this list when those epics add them to the API.
    const EXCLUDED: &[&str] = &[
        "storagePutObject",
        "storageUploadObject",
        "storageCreateUpload",
        "storageUploadPart",
        "storageCompleteUpload",
        "createFunctionDeployment",
    ];

    const READ: &[&str] = &[
        "storageListObjects",
        "getCell",
        "rehearseMerge",
        "storageResolveObjects",
        "getDatabaseConnection",
        "getBranchConnection",
        "storageDownloadObject",
    ];

    const WRITE: &[&str] = &["cancelSqlRun", "storageSignObject"];

    #[test]
    fn named_destructive_operations() {
        for id in DESTRUCTIVE {
            assert_eq!(class_of(id), Class::Destructive, "{id}");
        }
        for id in READ {
            assert_eq!(class_of(id), Class::Read, "{id}");
        }
        for id in WRITE {
            assert_eq!(class_of(id), Class::Write, "{id}");
        }
    }

    #[test]
    fn named_excluded_operations() {
        let excluded: BTreeSet<&str> = table::TABLE
            .iter()
            .filter(|(_, _, c)| *c == Class::Excluded)
            .map(|(id, _, _)| *id)
            .collect();
        let expected: BTreeSet<&str> = EXCLUDED.iter().copied().collect();
        assert_eq!(excluded, expected);
        for id in EXCLUDED {
            let method = table::TABLE.iter().find(|(op, _, _)| op == id).unwrap().1;
            for body in [None, Some(json!({})), Some(json!({"read_only": true}))] {
                assert_eq!(
                    table::classify(id, method, body.as_ref()),
                    Class::Excluded,
                    "{id}"
                );
            }
        }
    }

    #[test]
    fn rules_name_classified_operations_at_their_strictest() {
        for (id, sentence, _) in table::RULES {
            assert!(!sentence.is_empty());
            assert_eq!(class_of(id), Class::Destructive, "{id}");
            assert_eq!(table::rule_of(id), Some(*sentence));
        }
        let ruled: BTreeSet<&str> = table::RULES.iter().map(|(id, _, _)| *id).collect();
        let expected: BTreeSet<&str> = [
            "consoleQuery",
            "runSql",
            "storageCreateBucket",
            "applySchemaChanges",
        ]
        .into_iter()
        .collect();
        assert_eq!(ruled, expected);
    }
}
