# API follow-ups before step 6a

**Origin:** the API-client chore's end-of-step review raised three contract questions; the user decided them at the merge pause (2026-10-01). Spec: `docs/superpowers/specs/2026-10-01-api-client-design.md` (Part 2's error table and statuses are amended by the decisions below).

## Status

- **Current:** done; at the user's merge pause.
- **Decisions (user, 2026-10-01):**
  1. **422 for invalid input everywhere.** Malformed JSON (syntax), a wrong content type and a body over the limit keep 400 / 415 / 413; a well-formed body (or path/query value) with invalid values — names, bounds, formats — is **422 `invalid_request`** on every operation, `createDatabase` included.
  2. **Password resets answer 204 No Content** (both `resetDatabasePassword` and `resetBranchPassword`); the generated clients return `void`.
  3. **`CHELADB_MANAGEMENT_HOSTS` → `CHELADB_API_HOSTS`, `Scope::Management` → `Scope::Api`:** the hostname family serves all three API families (management, login, data). "Management" stays only for the `/v1` API itself (documents, client, `Auth::Management`, "management token").
- **Done:** Task 1 (`381dde0` rename to `CHELADB_API_HOSTS`/`Scope::Api`; `c8b26d0` 422 for invalid input everywhere, 204 resets, regenerated documents and clients; `d5c3328` `.env.example`). Review (sonnet): one Important (stale `.env.example` line), fixed and verified. Workspace 803/803, clippy clean, `just check` green, Studio 266/266, typecheck and build.
- **Ruling (accepted):** hand-parsed bodies (`parse_body`) answer 400 for empty or non-JSON bodies and 422 for well-formed JSON of the wrong shape, matching `ApiJson` and "malformed JSON stays 400". *Cost: one function and one assertion to revert.*
- **Minors:** Studio's reset tests weren't RED (the mutator already handled 204); createDatabase documents 415 while the hand-parsed routes don't check content type.

## Task 1: the three follow-ups

**Files:** `services/orchestrator/src/{lib.rs,branches.rs,data_keys.rs}`, `crates/cheladb-types/src/api_extract.rs` (if the extractor's 422/400 split needs it), `crates/cheladb-routes/src/lib.rs`, `services/gateway/src/{config.rs,registry.rs,pipeline.rs}`, tests, `README.md`, `openapi/*.json` and generated clients via `just api-gen`, Studio code that reads a reset response.

**Behaviour:**
- Audit every 400 the orchestrator returns: syntax errors (unparseable JSON, a non-UUID path segment) stay 400; semantic errors (a name or value that parses but is invalid) become 422. `createDatabase`'s "name is required" and "invalid database name" become 422. Update the annotations (`responses`) so each operation documents exactly what it returns.
- Both reset routes return `204` with no body; annotations `(status = 204, description = …)`; Studio's reset flows keep working (the generated function resolves `void`).
- Rename the scope and env var everywhere (readers, writers, tests, README); no other "management" changes.
- `just api-gen`, commit the regenerated documents and clients.

**Tests:** `invalid_names_are_422_everywhere` (createDatabase and createBranch with an invalid name → 422 `invalid_request`), `malformed_json_is_400` (unchanged), `password_resets_are_204` (both routes; empty body), gateway host-config tests renamed and green; Studio reset tests green.

**Verify:** `cargo nextest run --workspace`, clippy, `just check`, Studio `test`/`typecheck`/`build`, `rg -n "CHELADB_MANAGEMENT_HOSTS|Scope::Management"` finds nothing.
