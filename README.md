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
scripts/             e2e, stack and codegen scripts
docs/                security notes; roadmap/superpowers are archive only
AGENTS.md            agent workflow (epics, issues, audits)
```

## Prerequisites

- [proto](https://moonrepo.dev/docs/proto), which installs Rust, moon, node, pnpm, just and sccache from `.prototools`
- Docker (Compose runs the dependencies, and the worker starts each database's compute container)
- cmake: `sudo apt-get install -y cmake`
- clang and protoc: `sudo apt-get install -y clang protobuf-compiler`
- psql (PostgreSQL client): `sudo apt-get install -y postgresql-client`
- sccache as the Rust compiler wrapper: proto installs it, but the global registration and the `rustc-wrapper = "sccache"` line in `~/.cargo/config.toml` are one-time machine setup (see CONTRIBUTING.md, "Machine setup")
- [cargo-nextest](https://nexte.st), the Rust test runner behind `just test`: `cargo install --locked cargo-nextest`

## Install

To run Chelabase, use Docker Compose (Docker with Compose 2.30 or newer):

```bash
cd infra/compose
cp .env.example .env
docker compose up -d --wait
```

Then read the setup code with `docker compose logs init-secrets` and claim the instance at http://studio.chelabase.localhost:8000. Claim it before you expose the host. The full guide (settings, choosing your own secrets, SMTP, upgrades) is the Install page of the docs, `apps/docs/content/docs/self-hosting/install.mdx`.

## Development quickstart

The workbench is for developing Chelabase, not for running it. Self-hosters should use the install above.

```bash
git clone https://github.com/chelabase/chelabase.git
cd chelabase
proto install
pnpm install
just workbench up
```

`just workbench up` builds the Rust services on the host, starts the Compose dependencies, bootstraps Zitadel, then runs the services and Studio in containers, and prints the URLs. The first run builds a lot and takes a while.

Open http://studio.chelabase.localhost:8000 and choose **Sign in** as the bootstrap admin (`just zitadel-bootstrap` sets it, workbench only). Self-sign-up is off by default: an admin invites everyone else (or switches open sign-up on and approves requests). Emails such as invites and password resets land in Mailpit at http://mail.chelabase.localhost:8000.

Stop everything with `just workbench down`. `just` lists every recipe.

## Documentation

The docs site runs locally, reached through the gateway:

```bash
just workbench up --with docs
```

Then open http://docs.chelabase.localhost:8000/docs. It has three sections:

- [Using Chelabase](http://docs.chelabase.localhost:8000/docs/using/quickstart): projects, databases, branches, the data API, the TypeScript client, app users
- [Reference](http://docs.chelabase.localhost:8000/docs/reference/errors): the management and data API references and the error codes (the internal login API is under Architecture)
- [Architecture](http://docs.chelabase.localhost:8000/docs/architecture): how the gateway, Studio, data API, Neon storage and security fit together

Working on the repo: see [CONTRIBUTING.md](CONTRIBUTING.md) (worktrees and their stacks, tests, code generation). Product direction: [PRODUCT.md](PRODUCT.md).
