# pgls introspection queries

These 12 `.sql` files are copied byte-for-byte from
`github.com/supabase-community/postgres-language-server`, tag `0.26.0` (commit `653ecab1`),
`crates/pgls_schema_cache/src/queries/`. The MIT license of that project is in `LICENSE`.

They feed the SQL editor's language server (`Workspace.setSchema`), see `../schema.rs`, which
wraps each query in `json_agg` and maps the result to the JSON shape that version expects.

## Upgrading the pgls WASM build

1. Check out the new tag and diff `crates/pgls_schema_cache/src/queries/*.sql` against this folder.
2. Copy the changed files byte-for-byte; note the new tag and commit above.
3. Diff the structs in `crates/pgls_schema_cache/src/*.rs` (field names, enum variants, the
   `From` mappings) and update `assemble` in `../schema.rs` to match.
4. Regenerate `tests/fixtures/pgls_schema_<version>.json`, and load it with the new package's
   `setSchema` plus a `complete` call to prove the shape.
5. Run the ignored live test `tests/sql_schema_live.rs` (`just live`) and note the byte size.
