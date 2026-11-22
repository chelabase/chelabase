---
description: Plan and track a security-fix epic entirely locally, then publish it through a GitHub security advisory's private fork.
disable-model-invocation: true
argument-hint: "<goal> | <slug> | APPROVED <slug> | PUBLISH <slug>"
---

# /epic-security

Plan, track and publish an epic that fixes security defects. `chelabase/chelabase` is
public, so nothing about an unfixed defect goes to GitHub issues, PRs, pushed branches or
commit messages until the fixes are ready to merge. Don't implement product code in this
chat.

Task: `$ARGUMENTS`

Follow root `AGENTS.md` and the `issue-spec` skill. Everything lives in
`<main checkout>/.claude/.cache/epics/<slug>/` (git-ignored). Use that absolute path in
every brief, because worktrees don't have the cache.

## Writing rule: fixes, not exploits

Every file in the draft, every subagent prompt and everything that is published later
describes a problem by four things: where it is, the guarantee that is missing, the fix,
and the acceptance criteria that prove the fix holds. Never write attack steps, payloads,
proof-of-concept walkthroughs or "an attacker could…" narratives. Tests are named for the
behaviour that must hold. Ask sweep and review agents for defects and fixes, not exploits.

## Mode A: propose (`<goal>`, or `<slug>` with an existing draft)

Same as `/epic` Mode A, steps 1–7, with these differences:

- If the slug's cache directory already has `plan.md` or `findings.md`, continue from them.
  Don't start the plan again.
- Milestone bodies (`m<n>.md`) follow `issue-spec`. The `epic-meta` line gains
  `"local": true`. Where a body would link an issue or PR, name the local file instead.
- `findings.md` (when a sweep produced one) uses the writing rule above. Low findings that
  aren't in the epic stay in `backlog.md` in the cache. They are not filed as issues until
  the epic has shipped and the advisory is published.
- Stop for the human's OK. Do not touch GitHub.

## Mode B: apply (`APPROVED <slug>`)

Work from the draft as it is. If the draft is missing, stop.

1. Worktree: `just wt-add epic/<slug>` from the main checkout. **Don't push.** No empty
   commit is needed.
2. Write `state.json`:
   ```json
   {"local": true, "slug": "<slug>", "branch": "epic/<slug>",
    "milestones": {"1": {"status": "ready", "review": "whiteroom", "commits": [], "audit": null}}}
   ```
   Statuses follow `issue-spec` (`ready`, `building`, `landed`, `audited`,
   `changes-requested`, `blocked`). `state.json` takes the place of labels, issue comments
   and the PR checklist.
3. Report the waves, the draft path, and the next step: `/build <slug>`.

## Mode C: publish (`PUBLISH <slug>`)

Only after `/build` has finished the epic (every milestone `audited` or `landed`, the
whole-epic review written to `review.md`, and `just check` plus the live pass green). The
human must say `PUBLISH`.

1. **Squash.** In a new worktree, `just wt-add security/<slug>` from `main`, run
   `git merge --squash epic/<slug>`, and commit once. The message is fix-only:
   `fix(security): <summary>`, a bullet per milestone in fix terms, and the attribution
   trailer. No exploit detail.
2. **Advisory text.** Write `advisory.md` in the cache:
   - summary;
   - description: affected components, the fixes, and whether operators must upgrade or
     resync;
   - affected versions and the patched version;
   - severity or a CVSS vector;
   - credits.

   Use the writing rule. Show it to the human and wait for OK before creating anything on
   GitHub.
3. **Draft advisory.** As a draft, it is visible only to repository admins and the
   collaborators listed on it:
   ```bash
   gh api -X POST repos/chelabase/chelabase/security-advisories --input advisory.json
   ```
   `advisory.json` holds `summary`, `description`, `severity` (or `cvss_vector_string`) and
   `vulnerabilities: [{"package": {"ecosystem": "other", "name": "chelabase"},
   "vulnerable_version_range": "...", "patched_versions": "..."}]`. Record `ghsa_id` in
   `state.json`.
4. **Private fork.**
   `gh api -X POST repos/chelabase/chelabase/security-advisories/<ghsa_id>/forks`. Record
   the fork's `full_name` and `ssh_url`.
5. **Push and PR inside the fork.** `git push <ssh_url> security/<slug>`, then open a PR in
   the fork against its default branch (`gh pr create -R <fork full_name>`, or
   `gh api repos/<fork>/pulls` if `gh pr create` refuses). The PR body follows `pr-body`
   and the writing rule. GitHub Actions don't run in advisory forks; the local gate from
   `/build` is the test record, so paste it into the Test plan.
6. **Hand off.** Report the advisory URL and the fork PR. The human does the rest on the
   advisory page:
   - merge the PR into `main`;
   - publish and pin images from `main` (CONTRIBUTING "Releasing images");
   - request a CVE if wanted;
   - publish the advisory.

   After that, offer to file `backlog.md` as public issues and to remove the
   `epic-<slug>` and `security/<slug>` worktrees. Follow the merge-before-cleanup rule:
   never in the same message as the merge.

## Rules

- No `git push` to `origin`, no `gh issue`, `gh pr` or `gh label` calls on
  `chelabase/chelabase`, and no public comments, from Mode A until the human merges from
  the advisory. The only GitHub calls are the advisory and its fork in Mode C.
- Never share findings or drafts with an external service (gists, pastebins, web search
  queries that quote them).
- No product-code edits while planning.
- Never pass harness `isolation: "worktree"`.
