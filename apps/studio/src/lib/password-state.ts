import type { PasswordState } from "@cheladb/management-client";

/** What a role's password state means for the user; "Generated" is the only revealable one. */
export const PASSWORD_STATE_TEXT: Record<PasswordState, string> = {
  generated: "Generated",
  set_in_sql: "Password set in SQL — reset to get a revealable one",
  md5_unusable: "MD5 password: can't log in through Cheladb; reset it",
};
