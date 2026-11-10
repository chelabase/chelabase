---
name: issue-spec
description: Required body for every Chelabase issue — Problem, Outcome, Design sketch, Given/When/Then criteria, Tests to write, Out of scope, Verification and Review. Use whenever an issue is written, edited or read.
---

# Issue spec

**The bar:** a developer who has never seen the plan conversation, holding only this issue
and the repo, can implement it, prove it's done, and know what not to touch.

Milestone issues use this body, in this order. Humans filing issues use
`.github/ISSUE_TEMPLATE/`, which asks for the same sections.

## Title

A plain imperative summary, for example `Return 409 when a branch name is taken`. No
`type(area):` prefix — labels carry type and area. A bug is labelled `type:fix`; its title
says what should happen, not "bug: …".

## Body

````markdown
<!-- epic-meta {"epic": "<slug>", "milestone": <n>, "type": "feat", "scope": "<area>", "depends_on": [<n>], "touches": ["<glob>"], "wave": <w>, "review": "whiteroom|standard|none", "review_reason": "<why>", "model": "haiku|sonnet|opus", "model_reason": "<why>"} -->

## Problem

What is wrong or missing today, who hits it, and why it matters now. Two to five sentences.
Link the epic PR or the report that surfaced it.

## Outcome

What is observably true when this is closed — behavior a user, an operator or another
service can see. Two to five lines. No task list.

## Design sketch

- **Approach:** how it will be done, in a paragraph.
- **Touches:** the types, functions, endpoints, tables, components that change and how
  (new / changed / removed).
- **Contracts:** API, schema, config or token changes, and who consumes them (or "none").
- **Rejected:** at least one alternative and why not.

## Files & entry points

- Start reading at: `<file>:<symbol>`

## Acceptance criteria

- [ ] **Given** <state>, **when** <action>, **then** <observable result>.
- [ ] Each criterion is provable by someone who did not write the code.

## Tests to write

- `<test name or file::case>` — covers AC 1 (happy path)
- `<test name>` — covers AC 1 failure path (<the error case>)
- Manual: <exact steps>, only where an automated test is impractical, with the reason.

## Out of scope

- <Adjacent thing a reader would assume is included> -> <destination: #N, milestone m<n>, backlog>

## Verification

```bash
# crate/package tests and any named live tests for this milestone
```

## Review

Level: whiteroom|standard|none — <why>
Focus: <which guarantees the auditor should check first, when whiteroom>

## Notes

Optional pitfalls, links.
````

Follow-up issues outside an epic omit `milestone`, `wave` and `touches`, and keep `epic`
only if they belong to one.

## Rules

- **Acceptance criteria are checkable.** "Returns 409 on duplicate name" is; "error handling
  is improved" is not.
- **Tests to write name the cases**, including at least one failure path per behaviour.
  Layout-only Studio work may say "none — layout; browser check".
- **Out of scope is never empty**, and every exclusion names a destination.
- **Verification lists exact commands**, which `pr-body` copies into the PR test plan. Mark
  each live test (`just workbench test …`, `just <name>-e2e`) `shared-state` when it changes
  state other runs on the stack see (the same branch, role or database, or an app restart
  or rebuild).
- **Review is decided at planning time** per `AGENTS.md` (white-room rule).
- **Security issues are written as fixes.** Problem names the location and the missing
  guarantee; Outcome, Acceptance criteria and Tests to write state the behaviour that must
  hold. No exploit steps, payloads or proof-of-concept walkthroughs anywhere in the body.

## Labels

Every issue gets exactly one `type:*`, one `area:*`, one `status:*`, and `epic:<slug>` when
it belongs to an epic. White-room milestones also get `review:whiteroom`.

Status lifecycle: `ready` → `building` → `landed` → (`audited` | `changes-requested`) →
closed by the epic PR. Use `blocked` when waiting on another issue. Issues filed by humans
(templates) start at `triage` and move to `ready` once they meet this spec.

Create labels with `gh -R chelabase/chelabase label create --force` as needed during `/epic`:

| Label | Color hint |
|---|---|
| `epic:<slug>` | purple |
| `type:feat`, `type:fix`, `type:chore`, `type:docs`, `type:refactor`, `type:test`, `type:ci` | blue |
| `area:<name>` | gray |
| `status:ready`, `status:blocked`, `status:building`, `status:landed`, `status:audited`, `status:changes-requested`, `status:triage` | yellow / orange |
| `review:whiteroom` | red |

Areas in common use: `studio`, `gateway`, `orchestrator`, `worker`, `rest`, `auth`,
`realtime`, `metadata`, `neon`, `fork`, `docs`, `ci`, `deps`, `infra`.
