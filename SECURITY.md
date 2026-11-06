# Security policy

## Reporting a vulnerability

Please report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/chelabase/chelabase/security/advisories/new)
(the repository's **Security** tab → **Report a vulnerability**), or by email to
[security@chelabase.com](mailto:security@chelabase.com). Don't open a public issue, pull request
or discussion for a suspected vulnerability.

Include what you found, the affected component (gateway, auth, data API, Studio, the Neon or
Postgres forks, …), the version or commit, and steps to reproduce. We'll acknowledge the
report, keep you updated while we work on a fix, and credit you in the advisory unless you'd
rather stay anonymous.

## Supported versions

Chelabase is alpha software and not ready for production data. It has no tagged releases yet. Fixes land on `main`. A self-hosted install gets them once the
release images pinned in `infra/compose/docker-compose.yml` are updated and the install is
upgraded to them.
