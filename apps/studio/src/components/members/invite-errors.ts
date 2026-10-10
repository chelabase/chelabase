import { ApiError, type Project } from "@chelabase/management-client";
import { errorText } from "@/lib/errors";
import { refusalExtra, refusalMessage } from "@/lib/refusal-extra";
import type { GrantRow, RowError } from "./grant-rows";

/**
 * Where an invite or approve failure is shown: under the email, under a grant row, under the
 * delivery choice, or for the whole dialog.
 */
export interface InviteFailure {
  email?: string;
  delivery?: string;
  rows?: Record<string, RowError>;
  form?: string;
  /** The answer was `link_not_allowed` and email works: switch the delivery to email. */
  switchToEmail?: boolean;
}

/** Reads a failure by its error code (never its wording); anything unknown shows the server's message. */
export function inviteFailure(err: unknown, email: string, rows: readonly GrantRow[], projects: readonly Project[]): InviteFailure {
  if (!(err instanceof ApiError)) return { form: errorText(err) };
  const refusal = refusalExtra(err);
  const projectId = refusal.projectId;
  const row = projectId ? rows.find((r) => r.projectId === projectId) : undefined;
  const name = (id: string) => projects.find((p) => p.id === id)?.name ?? "The project";
  switch (err.code) {
    case "unavailable":
      return { form: "Couldn't reach the identity provider, so no invite was sent. Try again." };
    case "link_not_allowed":
      return err.extra.email_available === true
        ? { delivery: `${email} already has a way to sign in, so a link can't be used. Send the invite by email.`, switchToEmail: true }
        : { delivery: `${email} already has a way to sign in, so a link can't be used. Ask them to sign in with their existing account instead.` };
    case "signup_pending":
      return { email: `${email} signed up and is waiting for approval. Approve them under Requests.` };
    case "email_ambiguous":
      return { email: `More than one account in this organisation uses ${email}. Fix it in the identity provider first.` };
    case "email_in_use_elsewhere":
      return {
        email: `Another account outside this organisation already uses ${email}. Use a different email, or fix the account in the identity provider.`,
      };
    case "project_deleting":
      if (row) return { rows: { [row.projectId]: { message: `${name(row.projectId)} is being deleted.` } } };
      break;
    case "not_found":
      if (row) return { rows: { [row.projectId]: { message: `${name(row.projectId)} no longer exists.` } } };
      break;
    case "invalid_request": {
      if (row) {
        const dbId = refusal.databaseId;
        const database = dbId
          ? (projects.flatMap((p) => p.databases).find((d) => d.database_id === dbId)?.name ?? "a deleted database")
          : undefined;
        const { role, chain, branch, branchMain, why } = refusal;
        const message = refusalMessage({ role, chain, why }, database, branch, branchMain) ?? err.message;
        return { rows: { [row.projectId]: { message, role, chain, database, branch, branchMain } } };
      }
      if (!projectId && /email/i.test(err.message)) return { email: "Enter an email address." };
      break;
    }
  }
  return { form: errorText(err) };
}

/** A plausible email: something@something.something, no spaces. */
export function looksLikeEmail(v: string): boolean {
  return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(v);
}
