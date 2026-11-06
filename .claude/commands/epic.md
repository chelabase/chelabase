---
description: Plan an epic as milestone issues, then after APPROVED file them, open epic/<slug> and a draft PR.
disable-model-invocation: true
argument-hint: "<goal> | #<issue> | APPROVED <slug>"
---

# /epic

Plan (or file) an epic. Don't implement product code in this chat.

Task: `$ARGUMENTS`

Use `gh -R chelabase/chelabase`. Follow root `AGENTS.md` and the `issue-spec` / `pr-body` skills.

## Mode A — propose (no `APPROVED`)

1. Explore the code the goal touches. If the goal is `#<n>`, read that issue once.
   Where services must tell each other something (revocation, lifecycle events, cache
   invalidation, reconciles), weigh a NATS event (`crates/chelabase-nats`) against polling,
   reconcile loops or direct database reads, and record the choice in the design sketch
   (Approach or Rejected).
2. Pick a kebab-case slug (≤ 30 chars) and an epic title `type(area): summary`.
3. Cut milestones. Each one:
   - lands in one build run (aim ≤ ~400 changed lines);
   - is independently verifiable, with its own acceptance criteria and tests;
   - declares `touches` (narrow path globs), `depends_on`, `review` (`whiteroom|standard|none`
     per `AGENTS.md`), `review_reason`, `model` (`haiku|sonnet|opus`) and `model_reason`.
4. Shape for parallel work: shared foundations first, then disjoint slices. Hot shared files
   (Cargo.lock, workspace Cargo.toml, openapi regen, pnpm-lock) must not overlap in a wave.
   Two milestones whose live tests are marked
   `shared-state` (see `issue-spec` Verification) never share a wave.
5. Write draft issue bodies (`m1.md`, …) and a short `plan.md` (outcome, non-goals, risks,
   waves, milestone table) under `.claude/.cache/epics/<slug>/` (git-ignored). Bodies follow `issue-spec`.
6. **Red-team** (only when a milestone is `whiteroom`): one opus agent reads `plan.md`, the
   whiteroom bodies and the code they touch, and attacks them: bypasses, races, privilege
   paths, fail-open errors, and how the change interacts with the subsystems around it.
   At most 10 findings, each one line with a concrete scenario. Fold them into the bodies
   (an acceptance criterion, a test to write, or the Focus line). No separate document.
7. Report the epic brief, waves, milestones table (n, title, wave, depends, review, model,
   why), open questions, and the draft path. Stop for human OK. Do not create issues or PRs.

## Mode B — apply (`APPROVED <slug>`)

Work from the draft in `.claude/.cache/epics/<slug>/` as it is; don't re-plan. If it is missing, stop.

1. Create label `epic:<slug>` (`gh label create --force`).
2. Worktree: `just wt-add epic/<slug>` from the main checkout (directory `.worktrees/epic-<slug>`). Empty
   commit `chore: open epic <slug>`, push `-u`.
3. Create milestone issues in order from the draft bodies, with labels
   `epic:<slug>`, `type:<type>`, `area:<scope>`, `status:ready`, plus `review:whiteroom` when
   needed. Record each issue number.
4. Draft PR from `epic/<slug>` → `main`, body per `pr-body` (milestone checklist with real
   numbers, empty Review record, `Closes #…` per milestone). Title = epic title.
5. Report PR URL, issue numbers by milestone/wave, draft path, next step: `/build <slug>`.

## Rules

- No product-code edits while planning.
- Prefer `/quick` when every piece could land alone.
- Never pass harness `isolation: "worktree"`.
