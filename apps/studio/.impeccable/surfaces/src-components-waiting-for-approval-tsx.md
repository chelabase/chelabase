---
version: 1
slug: "src-components-waiting-for-approval-tsx"
primary_target: "src/components/waiting-for-approval.tsx"
related_targets: ["src/routes/app.tsx"]
---

# "Waiting for approval" screen

**Scope and mode:** Operate. Amendment (user, 2026-10-09; R39, R40), built by T12b. `src/components/waiting-for-approval.tsx`, shown by `routes/app.tsx` in place of the whole signed-in app when `/v1/me` answers `member.status === "pending"`. Every other API call answers 403 `pending_approval` for this caller, so nothing behind the shell would work: no rail, no header, one screen. The world is the inherited auth pages' (DESIGN.md: dark ground, the centred 24rem column of `LoginLayout`).

## 1. Job and audience

- Someone who created an account while open sign-up was on (self-hosted). Their account exists, but a tenant admin must approve it before they can use Studio. State of mind: "did it work?".

## 2. Outcome and proof

- They know the sign-up worked, which account they're signed in as, that a tenant admin decides, and that the page notices approval by itself. They can sign out (to switch accounts).
- Truths: `/v1/me` reports `member.status: "pending"`; tenant admins approve (with project access) or decline under Members → Requests. A declined account is deactivated, so the next `me` fails with 401/404 and the normal error or sign-in path takes over.

## 3. Selected direction

- Same frame as the auth pages (centred narrow column on ink, headline, short text, buttons). No illustration, no crab, no apology. Left-aligned text inside the column.

## 5. States and copy

- **Headline:** "Waiting for approval" (focused on first render, `tabIndex=-1`).
- **Body:** "You're signed in as {email}. A tenant admin of {tenant} needs to approve your account before you can use Chelabase Studio." `{email}` from `me.email` (else the token's; the clause "as {email}" is dropped when both are null). `{tenant}` is `me.tenant.display_name`, else its slug, else "this workspace".
- **Line:** chalk-dim "This page checks every 30 seconds and opens Studio as soon as you're approved."
- **Actions:** "Check now" (outline; "Checking…" and disabled while refetching), "Sign out" (ghost), which clears the query cache and runs the shell's sign-out.
- **Approved:** the next `me` shows another status; the app renders normally.
- **Refresh failed:** chalk-dim "Couldn't check just now ({errorText}); trying again." The screen stays.

## 6. Interaction and layout

- `refetchInterval` 30 s on `getMe` while mounted (TanStack pauses it in a hidden tab).
- Lagoon tokens only, dark only; Button from `components/ui/*`.

## 7. Related: the sign-in page (amendment 4)

- `/login` offers "Create account" only when the auth request's `signUpAllowed` is true. Otherwise it shows the generic "Email or password is incorrect" on a failed sign-in plus the standing line "No account? Ask your admin for an invite." (R37, no account enumeration). Built by T12y; the Studio landing (`/`) has only "Sign in", no "Create account".
