---
name: pr-body
description: Required PR body for Chelabase — Summary, Why, Changes, Test plan, Review record. Use whenever you write or check a PR description.
---

# PR body

PR title: `type(area): summary`. The body follows `.github/pull_request_template.md`.

## Epic PR

```markdown
## Summary

- <2–4 bullets: what this epic does, in user-visible terms>

## Why

<The problem, linking the goal issue if any. One short paragraph.>

## Changes

- <area>: <what changed and why that way>

## Milestones

- [ ] m1 — <title> (#12)
- [ ] m2 — <title> (#13)

## Test plan

### Milestone 1 — <title> (#12)
- [ ] `<verification command>`
- [ ] <heavier check> — not required: <why>

### Epic-wide
- [ ] `just check`
- [ ] <live pass suites touched by this epic>

## Review record

| Milestone | Review | Result | Audit SHA | Notes |
|---|---|---|---|---|
| m1 (#12) | whiteroom | | | |
| m2 (#13) | standard | — | — | covered by epic review |
| m3 (#14) | none | — | — | docs only |

Closes #12
Closes #13
```

Add `## Screenshots` only when an issue's acceptance criteria ask for them.

## Quick PR

Flat Test plan (no milestone headings), no Review record. `Closes #N` when it closes an
issue. If the change touches a white-room path, say which and why `/quick` was still OK
(or refuse and point at `/epic`).

## Who fills what

- **`/epic`** writes the skeleton: unchecked milestone boxes, empty Review rows, `Closes #…`.
- **`/build`** checks boxes only for commands it actually ran; fills a milestone's Test plan
  section when that milestone lands; fills Review row `none` / `standard` at integrate or
  at the epic-end review.
- **`/audit`** fills the whiteroom Review row for its milestone.
- Never check a box for a command you didn't run in this session.

## Stack checks

Heavier checks (`just <name>-e2e`, named live Rust tests via `just workbench test`) run only
when their condition holds. Anything not run goes in the Test plan as `not required: <why>`.
