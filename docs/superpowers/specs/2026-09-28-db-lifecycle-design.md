# Database Lifecycle — Design Spec

**Date:** 2026-09-28  
**Status:** Approved (in conversation)  
**License:** Apache-2.0  
**Depends on:** [2026-09-27-studio-auth-design.md](./2026-09-27-studio-auth-design.md)

## Intent

Make a database usable end to end: create it, get connection details you can show again later, reset its password, and delete it. Along the way:
- stop exposing the Postgres superuser password
- clean up after failed or abandoned work
- actually remove data on delete

## Decisions

- **Two roles per database, Supabase-style:**
  - **`cheladb_platform`** is a true superuser, never revealed, and used only by the platform for lifecycle work.
  - **`postgres`** is the owner role users receive. It is elevated but not a superuser.
  - An operator switch, `CHELADB_DB_OWNER_SUPERUSER=true`, makes the owner a true `SUPERUSER` for new databases, accepting that users can then break platform management.
- **Connection details can be revealed again**, so passwords are stored **encrypted at rest** (not hashed) with a control-plane key.
- **Reset password** is included.
- **Delete leaves a tombstone:** the row stays as `deleted` and is hidden from the API and UI, and the name becomes reusable.
- **Connections still go to `127.0.0.1:<port>`.** The Postgres wire proxy (`pgproxy`) replaces that later and will authenticate against this credential model. Postgres traffic never goes through the HTTP gateway: they are two front doors, one per protocol.

## Roles inside each database

- **Platform role.** The container starts with `POSTGRES_USER=cheladb_platform`, a random 32-character password, and `POSTGRES_DB=<sanitized name>`.
- **Owner role.** Once Postgres accepts connections, the worker connects as `cheladb_platform` and runs:

```sql
CREATE ROLE postgres LOGIN PASSWORD '<owner password>' CREATEDB CREATEROLE BYPASSRLS;  -- + SUPERUSER when the switch is on
GRANT pg_read_all_data, pg_write_all_data, pg_monitor, pg_signal_backend TO postgres;
ALTER DATABASE "<db>" OWNER TO postgres;
-- connected to <db>:
ALTER SCHEMA public OWNER TO postgres;
```

- **Why the owner can't break platform access:** in Postgres 17, `CREATEROLE` only lets the owner manage roles it created, so it can't alter or drop `cheladb_platform`.
- **Extensions:** the owner can install trusted extensions (e.g. `pgcrypto`) because it owns the database.
- **Password format:** alphanumeric only, so the passwords embed safely in SQL literals and URIs.
- **Storage:** each database gets a named data volume, `cheladb-db-<database_id>`, labelled `cheladb.managed=true` and mounted at `/var/lib/postgresql/data`. That makes delete and cleanup explicit; today it's an anonymous volume that leaks.

## Secrets at rest (`crates/cheladb-secrets`)

- **Encryption:** AES-256-GCM with a random 96-bit nonce. The database id is the associated data, so a ciphertext only decrypts for its own row.
- **Stored format:** `v1:` + base64(nonce ‖ ciphertext).
- **Key:** 32 random bytes, base64, in a file named by `CHELADB_SECRETS_KEY`.
  - `cheladb auth bootstrap` creates it at `.cheladb/secrets/secrets.key` (0600, created once, never overwritten) and writes the path into `demo.env`.
  - The worker needs the key to encrypt at provisioning and reset time; the orchestrator needs it to decrypt on reveal.
- Rotating the key is out of scope.

## Data model (migration)

`databases` gains these columns:

| Column | Purpose |
| --- | --- |
| `database_name` | Actual Postgres database name (sanitized) |
| `owner_role` | Revealed role, `postgres` |
| `owner_password_enc` | Encrypted owner password |
| `platform_password_enc` | Encrypted platform password |
| `deleted_at` | Set when the row becomes a tombstone |

- `admin_password` (plaintext) is **dropped**. Databases created before this change have no stored credentials, and reveal answers `409` with "credentials unavailable; recreate this database".
- New status value `deleted`, joining the existing `pending`, `provisioning`, `ready`, `failed` and `deleting`.
- `UNIQUE (project_id, name)` becomes a partial unique index `WHERE status <> 'deleted'`, so names are reusable after delete.

## API

All endpoints are declared in `cheladb-routes` (`Auth::Platform`, `Cors::Studio`) and tenant-checked like `GET /v1/databases/{id}`.

| Endpoint | Behavior |
| --- | --- |
| `GET /v1/databases/{id}/connection` | `200 { host, port, database, user, password, uri }` with `Cache-Control: no-store`. `uri` is `postgres://postgres:<pw>@<host>:<port>/<db>`. Answers `409` unless `ready` or when credentials are unavailable, and `404` if deleted. Logs `request_id`, `sub` and `database_id`, never the password. |
| `POST /v1/databases/{id}/reset-password` | `200 {}` once the new password is applied and stored. Answers `409` unless `ready`, `404` if deleted, and `504` if the worker doesn't reply within 15 s. |
| `DELETE /v1/databases/{id}` | `202 { status: "deleting" }`. Answers `409` if already `deleting` and `404` if deleted (or not found). Allowed from `pending`, `provisioning`, `ready` and `failed`. |
| `GET /v1/databases/{id}` | No longer returns any password or `container_id`. `404` for deleted. |
| `GET /v1/me` | Omits deleted databases. |

## Messaging

- **Delete:** a durable JetStream job on a new subject, `cheladb.delete`, consumed by the worker. Same envelope and ack pattern as provisioning.
- **Reset password:** a NATS core request/reply on `cheladb.rpc.reset_password` (`{ database_id }` → `{ ok } | { error }`). It's short and interactive, and it needs the worker because only the worker talks to tenant databases.

## Worker

- **Provision:**
  - Uses the named volume and the platform role, then creates the owner role.
  - Encrypts both passwords and stores them with host, port, `database_name` and `owner_role`.
  - Before marking the database `ready`, re-reads its status. If it became `deleting` or `deleted` meanwhile, the worker tears everything down instead.
- **Delete:**
  - Stops and removes the container and the volume.
  - Sets `status = deleted` and `deleted_at = now()`, and clears host, port, `container_id` and both encrypted passwords.
  - Safe to repeat.
- **Reset password:**
  - Decrypts the platform password, connects as `cheladb_platform` and runs `ALTER ROLE postgres PASSWORD '<new>'`.
  - Stores the new encrypted owner password, then replies.
  - If the database isn't `ready`, it replies with an error.
- **Reconcile** runs every 30 s in the worker, as a single worker for now:

  | Found | Action |
  | --- | --- |
  | `pending` / `provisioning` older than 10 minutes | Mark `failed` ("provisioning timed out") and remove its container and volume |
  | `ready`, but its container is gone | Mark `failed` ("container missing") |
  | `deleting` older than 60 s (lost job) | Run the delete again |
  | Managed container or volume with no live row (none, or `deleted`) | Remove it |

  The decisions come from a pure function over rows, containers and volumes, so they can be tested without Docker.

## Studio

Each database row gets three actions:
- **Connection details** reveals the details on demand. The password is masked until you choose Show, there are copy buttons for each field and the URI, and it has a Hide option. It's disabled unless `ready`.
- **Reset password** asks for confirmation, then shows the fresh details.
- **Delete** asks you to type the database name. The row then shows `deleting` until it disappears.

## Housekeeping

`just sweep-dbs` also removes managed volumes; today they leak.

## Testing

- **`cheladb-secrets`:**
  - an encrypt/decrypt round trip
  - a wrong key is rejected
  - a swapped database id is rejected
  - tampering is rejected
  - a malformed stored value is rejected
- **Worker:**
  - owner-role SQL, with and without the superuser switch
  - reconcile decisions, as a pure function
  - database-name sanitizing
- **Orchestrator** (with fakes):
  - reveal: ready, not ready, other tenant, deleted, credentials unavailable
  - reset: ok, not ready, worker timeout
  - delete: `202`, `409`, `404`
  - `GET` and `/v1/me` never contain passwords, and hide deleted databases
- **Studio:**
  - reveal masking and copying
  - reset confirmation
  - delete type-to-confirm
  - `deleting` state
- **End to end, against Docker:**
  - create a database, reveal its details, `psql` in as `postgres`
  - check it is not a superuser but can create a table and `pgcrypto`
  - reset the password: the old one fails and the new one works
  - delete it: container and volume are gone, the row is `deleted`, and the name is reusable
  - reconcile removes a hand-made orphan container

## Out of scope

`pgproxy` and SNI routing, TLS to databases, secret-key rotation, a persistent audit log, backups, database branching, deleting tenants or projects, and roles or permissions in Studio.
