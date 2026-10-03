# PR draft (not opened)

Branch: ChelaDB/rust-postgres `upstream/simple-query-tags` (8bf6bc1), based on upstream master 6c161e32.

## Title

tokio-postgres: expose command tags, empty queries and column types in simple queries

## Body

### Motivation

`SimpleQueryMessage::CommandComplete(u64)` only carries a row count, an empty query is reported as `CommandComplete(0)`, and `SimpleColumn` only carries the name. Clients that run arbitrary SQL (SQL editors, admin tools) need the full command tag (`INSERT 0 2`, `CREATE TABLE`, `BEGIN`), a way to tell an empty statement from a zero-row one, and the column types.

### API

- `SimpleQueryStream::with_command_tags(self) -> SimpleQueryStream`: opt-in. Completed statements are reported as `SimpleQueryMessage::CommandTag(SimpleCommandTag)` and empty statements as `SimpleQueryMessage::EmptyQuery`, instead of `CommandComplete`.
- `SimpleCommandTag` (`Debug, Clone, PartialEq, Eq`): `tag() -> &str`, `rows() -> u64` (0 when the tag has no count).
- `SimpleColumn::type_oid() -> Oid` and `SimpleColumn::type_modifier() -> i32` (-1 when none; see `pg_attribute.atttypmod`).

```rust
let mut stream = client
    .simple_query_raw("INSERT INTO t VALUES (1), (2); ;")
    .await?
    .with_command_tags();
while let Some(msg) = stream.try_next().await? {
    match msg {
        SimpleQueryMessage::CommandTag(t) => println!("{} ({} rows)", t.tag(), t.rows()), // INSERT 0 2 (2 rows)
        SimpleQueryMessage::EmptyQuery => println!("empty"),
        _ => {}
    }
}
```

### Compatibility

- No behaviour change unless `with_command_tags` is called; `simple_query` and default streams still yield `CommandComplete`, including `CommandComplete(0)` for empty queries.
- `SimpleQueryMessage` is already `#[non_exhaustive]`, so the new variants are not a breaking change.
- No new dependencies. No CHANGELOG entry, since master has no Unreleased section.

### Tests

- `simple_query_command_tags`: tags and row counts for CREATE TABLE / INSERT / SELECT / BEGIN / COMMIT (rows 0 where no count); no `CommandComplete` emitted; `INT4`/`TEXT`/`VARCHAR` OIDs; `type_modifier` is -1 for `INT4` and 14 for `VARCHAR(10)`.
- `simple_query_empty_query`: `;` and a comment-only query yield `EmptyQuery` when opted in and `CommandComplete(0)` by default.

---
This change was prepared with Claude Code.
