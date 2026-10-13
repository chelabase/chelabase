# Chelabase

Chelabase is a database platform you run yourself: Postgres databases with copy-on-write branches and point-in-time restore (on Neon's storage engine), a typed data API with per-branch keys, login roles, app-user auth and a web console (Studio). One gateway fronts every API.

Licensed under Apache-2.0.

## Repository layout

```
apps/studio          Studio, the web console
apps/docs            the documentation site
services/            gateway, orchestrator, worker, rest, auth
tools/cli            the `chelabase` binary (dev bootstrap, type generation)
crates/              shared Rust libraries (routes, types, metadata, query, ...)
packages/            TypeScript packages: client, management-client, neon-e2e
openapi/             generated OpenAPI documents (management, login, data)
infra/compose/       metadata Postgres, NATS, Zitadel, Mailpit, Neon storage
infra/neon/          Neon image pins and build notes
scripts/             dev stack, live queue and codegen scripts
docs/                design specs, plans, roadmap and security notes
```

## Prerequisites

- [proto](https://moonrepo.dev/docs/proto), which installs Rust, moon, node, pnpm and just from `.prototools`
- Docker (Compose runs the dependencies, and the worker starts each database's compute container)
- cmake: `sudo apt-get install -y cmake`
- clang and protoc: `sudo apt-get install -y clang protobuf-compiler`
- psql (PostgreSQL client): `sudo apt-get install -y postgresql-client`

## Quickstart

```bash
git clone https://github.com/chelabase/chelabase.git
cd chelabase
proto install
pnpm install
just dev
```

`just dev` starts the Compose dependencies, bootstraps Zitadel, builds the Rust services and runs them and Studio in the background. The first run builds a lot and takes a while.

Open http://localhost:3000 and choose **Sign in** as the bootstrap admin (`just zitadel-bootstrap` sets it). Self-sign-up is off by default: an admin invites everyone else (or switches open sign-up on and approves requests). Emails such as invites and password resets land in Mailpit at http://localhost:8025.

Stop everything with `just stop`. `just` lists every recipe.

## Documentation

The docs site runs locally and needs no stack:

```bash
just docs
```

Then open http://localhost:3001/docs. It has three sections:

- [Using Chelabase](http://localhost:3001/docs/using/quickstart): projects, databases, branches, the data API, the TypeScript client, app users
- [Reference](http://localhost:3001/docs/reference/errors): the management and data API references and the error codes (the internal login API is under Architecture)
- [Architecture](http://localhost:3001/docs/architecture): how the gateway, Studio, data API, Neon storage and security fit together

Working on the repo: see [CONTRIBUTING.md](CONTRIBUTING.md) (worktrees, the live queue, tests, code generation). Product direction: [PRODUCT.md](PRODUCT.md).
