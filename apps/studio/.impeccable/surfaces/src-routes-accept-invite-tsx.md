---
version: 1
slug: "src-routes-accept-invite-tsx"
primary_target: "src/routes/accept-invite.tsx"
related_targets: ["src/components/login/sign-in-form.tsx"]
---

# Accept invite page

**Scope and mode:** Operate. The public route `/accept-invite?user={zitadel user id}&code={invite code}&t={Chelabase invite token}` (`src/routes/accept-invite.tsx`), built on the existing auth page frame (`LoginLayout`, `Field`, `Problem` from `components/login/sign-in-form.tsx`, wrapped in `LoginProviders`). It calls the auth service's `POST /auth/v1/accept-invite` (operation `acceptInvite`, login client) and, to name who invited them (Q10, user), the new public `GET /auth/v1/invite?user&t`. Shape brief (Members step T12a); the world is the inherited auth pages' (DESIGN.md: dark ground, auth pages included). The user's answers to the T12a questions are applied.

## 1. Job and audience

- A person who just clicked the link in an invite email (Zitadel sends it; Mailpit in dev). Often new to Chelabase, arriving cold from their inbox. Some are coming back after being removed (R29: they return as invited with a fresh code).
- Job: set a password and land signed in, in one step.

## 2. Outcome and proof

- Success: one password field, one button, then Studio opens signed in. The response's `redirect` is followed (Studio's address when there's no pending sign-in).
- Truths from the API:
  - Body: `{ user, code | acceptTicket, password, authRequest? }`. The link carries no `authRequest`; the page sends none.
  - `200`: signed in, session cookie set, `{ next: "done", redirect }`.
  - `400 invalid_request`: wrong code or ticket, or a malformed body. `400 weak_password`: the password is refused, **with `error.accept_ticket` once the code was spent** (R13).
  - `503 invite_incomplete` with `error.accept_ticket`: a step after the code check failed; retry with the ticket instead of the spent code (R13). Tickets are one-use and last 15 minutes.
  - `410 invite_expired` / `410 invite_revoked`. `429 rate_limited`. `503 unavailable`.
  - Q10 (user; new endpoint): `GET /auth/v1/invite?user={user}&t={t}` returns `{ inviter_name, inviter_email, tenant_name }`. `t` is a Chelabase token carried in the invite link (not the Zitadel code). Every failure is the same 404, so the page can't tell why it failed and never says.
- The password never enters the query or mutation cache: the mutation uses `SECRET` options and `reset()` after each call, as the other auth forms do; the password and the ticket live only in component state.

## 3. Selected direction

- Same frame as sign-in and password reset, so the invitee recognises one product: the centred 24rem column on ink, headline, fields, full-width primary button. No new layout. The only addition is a line naming who invited them to which tenant, when the lookup succeeds.
- Thesis: **one field, one press.** Everything else is error recovery, written so they never need to ask what went wrong.

## 4. Scope and boundaries

- The page, its states, and its R13 retry behaviour (T12b test `retries_with_accept_ticket`).
- Untouched: the other auth pages, `LoginLayout` itself.
- Anti-goals: an email field (the invite fixes the account; the page doesn't know the email, only the Zitadel user id); a "confirm password" field (the other auth forms use one field; Zitadel's policy answers `weak_password`); asking for a name.

## 5. States and ranges

- **Ready** (both `user` and `code` present):
  - Title "Join Chelabase".
  - **Who invited them** (Q10): while the lookup runs, nothing extra is shown (no placeholder, no spinner); the form is usable at once and never waits on it. When it succeeds, a line above the form: "{inviter} invited you to {tenant_name}." where {inviter} is `inviter_name`, followed by `inviter_email` in chalk-dim in brackets, or the email alone when the name is empty. When it fails (404, network, missing `t`), the line is simply absent: the generic page, no error shown.
  - Line: "Set a password to accept your invite. You'll use it with your email to sign in."
  - Field "Password" (`type=password`, `autocomplete=new-password`, required).
  - Button "Set password and sign in" (full width, primary).
- **Submitting:** button "Setting your password…", disabled; field read-only.
- **Success:** `window.location.assign(redirect)`; nothing else rendered (the button stays in its busy state until the browser leaves).
- **Link incomplete** (`user` or `code` missing or empty; a missing `t` only skips the inviter line), shown without a form: title "This invite link is incomplete". "Open the link from your invite email again. If it still doesn't work, ask the person who invited you to send a new invite." Link "Back to Chelabase Studio" (`/`).
- **`weak_password`** (400): the server's message under the field (it names the rule). If `accept_ticket` came back, keep it: the next submit sends `acceptTicket` in place of `code`. The field is cleared and focused.
- **`invite_incomplete`** (503, with `accept_ticket`): keep the ticket. Problem line: "Your invite code is confirmed, but setting up your account didn't finish. Try again." The button reads "Try again" and resubmits `{ user, acceptTicket, password }` with the password still in component state. If the field was cleared by an earlier error, it asks for the password again.
- **Wrong code or spent ticket** (`400 invalid_request`): no form. Title "This invite link doesn't work". "It may be an older link: only the newest invite email works. Ask the person who invited you to send it again." When it failed while retrying with a ticket: "Setting up your account didn't finish in time. Try signing in with your email and the password you chose; if that fails, ask for a new invite." with a "Sign in" link to `/app` (which starts the normal sign-in).
- **`invite_expired`** (410): no form. Title "This invite has expired". "Invites work for 72 hours. Ask the person who invited you to send it again; the new email has a fresh link."
- **`invite_revoked`** (410): no form. Title "This invite was withdrawn". "If you still need access, ask a tenant admin to invite you again."
- **`rate_limited`** (429): Problem line "Too many attempts. Wait a minute, then try again." The form stays.
- **`unavailable`** and network failures: the shared `messageOf` wording, "Sign-in is unavailable, try again". The form stays; the code isn't spent.
- **Already signed in** as someone else: not detected by this page (no session lookup); the accept signs the browser in as the invitee. Recorded, not designed around.

## 6. Interaction and layout

- `LoginLayout` (centred, `max-w-sm`, `p-8`), headline 1.5rem/600, the line in chalk-dim 14px, fields per `Field`, errors per `Problem` (`role=alert`), all on the lagoon ink ground.
- Focus starts in the password field. After an error that keeps the form, focus returns to the field. Terminal states (incomplete link, expired, revoked, wrong code) move focus to the title.
- Enter submits; double submits are blocked by the busy flag (as in the other forms).
- The code, `t` and the ticket are never written to `localStorage`, the URL (beyond the original link) or logs.
- The inviter lookup is a plain query fired once on load (no retries, no refetch on focus). The inviter line arrives without moving the form: it sits above the explanatory line and the form doesn't shift focus when it appears.

## 7. Constraints and open decisions

- Lagoon tokens only, dark only; `components/ui/*` plus the login frame components.
- Components used: LoginLayout, Field, Problem, Button. Nothing new. (`Field`'s `id` is derived from its label; one field here, so no clash.)
- Q10 (user): the inviter and tenant are named through `GET /auth/v1/invite?user&t` (new; `t` is added to the invite link's URL template). Never block the form on it; a failed lookup shows the generic page.
- Amendment (R35): the same page serves one-time links an admin copied and sent by hand (link delivery); they carry the same `user`, `code` and `t`. Nothing changes on the page.
- Amendment (sign-in): `/login` shows "Create account" only when `signUpAllowed`; otherwise "No account? Ask your admin for an invite." The landing (`/`) has no "Create account". See the waiting-for-approval brief, section 7.
