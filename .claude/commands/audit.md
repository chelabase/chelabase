---
description: White-room audit of a landed milestone — issue + diff only; posts pass or changes-requested.
disable-model-invocation: true
argument-hint: "<slug> <n>"
---

# /audit

Independent audit of milestone `n` on epic `<slug>`. Don't audit or fix in this chat.

Task: `$ARGUMENTS`

1. Confirm the issue is `status:landed` and carries `review:whiteroom` (or the human asked).
2. Launch the **auditor** agent (`.claude/agents/auditor.md`) in the background. Prompt:
   `project: chelabase`, `epic: <slug>`, `milestone: <n>`, issue number, and nothing else
   about the builder's reasoning. Model: `opus` for auth/crypto/fork/migration; `sonnet`
   otherwise. Never pass `isolation: "worktree"`.
3. Report the verdict (`pass` / `changes-requested`), findings by severity, and next step
   (`/build <slug>` to continue, or `/build <slug> <n>` to fix).
