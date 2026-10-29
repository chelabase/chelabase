# Chelabase

Chelabase is a database platform you run yourself: Postgres databases with copy-on-write branches, a typed data API, login roles, app-user auth and a web console, on Neon's storage engine.

<!-- Static badges: the repo is private and has no release or CI yet. When the repo is public, the
first release is cut and CI (#7) exists, swap in dynamic release and CI badges. -->
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![PostgreSQL 17](https://img.shields.io/badge/PostgreSQL-17-336791)](infra/neon/README.md)
[![Self-hosted: Docker Compose ≥ 2.30](https://img.shields.io/badge/self--hosted-Docker%20Compose%20%E2%89%A5%202.30-2496ed)](apps/docs/content/docs/self-hosting/install.mdx)
[![Status: pre-release](https://img.shields.io/badge/status-pre--release-orange)](SECURITY.md#supported-versions)

## What it is

- **Postgres on Neon's storage engine.** Each database gets its own Neon tenant, timeline and compute. Branches are copy-on-write, and a branch can be made or restored at a point in time.
- **A typed data API.** A JSON query endpoint with row-level security, per-branch keys, generated TypeScript types and a client, `@chelabase/client`.
- **Login roles** for connecting with `psql` and Postgres drivers, with encrypted, re-revealable credentials.
- **App-user auth:** sign-up, sign-in and sessions for your app's own users, with helpers for RLS policies.
- **Studio**, the web console: projects, databases, branches, tables, the SQL editor, keys, members and app users.

One gateway fronts Studio, the APIs and sign-in.

## Requirements

- Docker Engine 26.0 or newer (the install mounts volume sub-paths).
- Docker Compose 2.30 or newer. Check with `docker compose version`.
- A checkout of this repository, for `infra/compose`.

Running Chelabase needs nothing else on the host. The [Install](apps/docs/content/docs/self-hosting/install.mdx#requirements) page lists what the machine needs.

## Install

```bash
git clone https://github.com/chelabase/chelabase.git
cd chelabase/infra/compose
cp .env.example .env
docker compose up -d --wait
```

The defaults run on one machine and serve Studio at <http://studio.chelabase.localhost:8000>.

> [!WARNING]
> **Claim the instance before you expose the host.** A new instance has no admin: the first account that signs up with the setup code becomes the admin of the whole instance. The gateway is published on `127.0.0.1` only by default. Keep it that way until you have claimed it.

1. Read the setup code (`Setup code: XXXX-XXXX-XXXX-XXXX-XXXX`):

   ```bash
   docker compose logs init-secrets
   ```

2. Open <http://studio.chelabase.localhost:8000>, choose **Sign in**, then **Create account**, and enter the code. You are now the instance's admin.

After the claim, sign-up is closed: you invite everyone else, or switch open sign-up on and approve requests.

The [Install](apps/docs/content/docs/self-hosting/install.mdx) page covers the rest: the settings, choosing your own secrets, your own domain with TLS behind a reverse proxy ([Hosts](apps/docs/content/docs/self-hosting/hosts.mdx)), email (SMTP), upgrades, and stopping or uninstalling.

## First database

1. In Studio, create a project (**New project**), then a database in it. It shows as provisioning, then as ready.
2. Create a table, in the SQL editor or the table editor.
3. In the database's Data API view, copy the public key (role `anon`, safe in a browser) or create a secret key (server only; it is shown once).
4. Get the generated types, `db.ts`: in the same Data API view, choose **Download types**. Or fetch them with a secret key, from any build step:

   ```bash
   export CHELABASE_DATA_KEY=chk_sec_…   # a secret key: the types route refuses a public one
   curl -fsS -H "Authorization: Bearer $CHELABASE_DATA_KEY" \
     http://api.chelabase.localhost:8000/data/v1/types -o db.ts
   ```

5. Query the database from your app with `@chelabase/client`. In a browser, use the public key and the `types` from `db.ts`:

   ```ts
   import { createClient } from "@chelabase/client";
   import { type Database, types } from "./db";

   const db = createClient<Database>({ url: "http://api.chelabase.localhost:8000", key: "chk_pub_…", types });

   const rows = await db.from("orders").select("id", "total").where({ status: "paid" }).limit(10);
   ```

   On a server, keep the secret key there and fetch the runtime types when the server starts, so value decoding follows the live schema without a rebuild:

   ```ts
   import { createClient, fetchTypes } from "@chelabase/client";
   import type { Database } from "./db";

   const url = process.env.CHELABASE_URL!;
   const key = process.env.CHELABASE_DATA_KEY!; // chk_sec_…

   const db = createClient<Database>({ url, key, types: await fetchTypes({ url, key }) });
   ```

The client is not on npm yet: it is a workspace package in this repository (`packages/client`, added as `"@chelabase/client": "workspace:*"`). Getting the types needs only Studio or `curl`. Without them, `createClient({ url, key })` works untyped. See the [TypeScript client](apps/docs/content/docs/using/client/index.mdx), [Generated types](apps/docs/content/docs/using/client/types.mdx) and the [data API](apps/docs/content/docs/using/data-api/index.mdx).

For migrations, ORMs and admin tools, connect to Postgres directly with the credentials in the database's **Connection details** in Studio; the [Install](apps/docs/content/docs/self-hosting/install.mdx) page says how connections reach the instance from the host. The [Quickstart](apps/docs/content/docs/using/quickstart.mdx) walks through all of it.

## Documentation

The docs are in this repository, under [`apps/docs/content/docs/`](apps/docs/content/docs/). Start with the [Introduction](apps/docs/content/docs/index.mdx).

- **Using:** the [Quickstart](apps/docs/content/docs/using/quickstart.mdx), [projects and databases](apps/docs/content/docs/using/projects-and-databases.mdx), [branches](apps/docs/content/docs/using/branches.mdx), [login roles](apps/docs/content/docs/using/login-roles.mdx), the [data API](apps/docs/content/docs/using/data-api/index.mdx), the [TypeScript client](apps/docs/content/docs/using/client/index.mdx), [app users](apps/docs/content/docs/using/app-users/index.mdx), [members and invites](apps/docs/content/docs/using/members-and-invites.mdx) and [Studio](apps/docs/content/docs/using/studio.mdx). The rest is in [`using/`](apps/docs/content/docs/using/).
- **Self-hosting:** [Install](apps/docs/content/docs/self-hosting/install.mdx) and [Hosts](apps/docs/content/docs/self-hosting/hosts.mdx).
- **Reference:** the [error codes](apps/docs/content/docs/reference/errors.mdx) and the [access per route](apps/docs/content/docs/reference/access.mdx). The management and data API references are generated from the OpenAPI documents in [`openapi/`](openapi/).
- **Architecture:** [how the parts fit together](apps/docs/content/docs/architecture/index.mdx): the gateway, Studio, the data API, Neon storage and [security](apps/docs/content/docs/architecture/security.mdx).

## Contributing

To work on Chelabase itself, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

Report vulnerabilities privately, as [SECURITY.md](SECURITY.md) describes.

## License

[Apache-2.0](LICENSE).
