import { ApiError, type FunctionAuth } from "@chelabase/management-client";
import { errorText } from "./errors";
import { actAsReserved } from "./role-rules";

/**
 * The enable switches' optimistic state for one branch (the scope): a flipped switch shows its new
 * value at once (`overrides`) until the API answers; a refusal drops the override, so the switch
 * reverts to the server's value, and keeps the reason (`errors`). A new scope starts empty, and an
 * answer for another scope (a branch left before it came back) changes nothing.
 */
export interface TogglesState {
  scope: string;
  overrides: Record<string, boolean>;
  errors: Record<string, string>;
}

export type TogglesAction =
  | { type: "flip"; scope: string; name: string; enabled: boolean }
  | { type: "settled"; scope: string; name: string }
  | { type: "failed"; scope: string; name: string; error: string }
  | { type: "scope"; scope: string };

export function initialToggles(scope: string): TogglesState {
  return { scope, overrides: {}, errors: {} };
}

function without<T>(record: Record<string, T>, key: string): Record<string, T> {
  const { [key]: _drop, ...rest } = record;
  return rest;
}

export function togglesReducer(state: TogglesState, action: TogglesAction): TogglesState {
  if (action.type === "scope") return action.scope === state.scope ? state : initialToggles(action.scope);
  if (action.scope !== state.scope) return state;
  switch (action.type) {
    case "flip":
      return { ...state, overrides: { ...state.overrides, [action.name]: action.enabled }, errors: without(state.errors, action.name) };
    case "settled":
      return { ...state, overrides: without(state.overrides, action.name) };
    case "failed":
      return { ...state, overrides: without(state.overrides, action.name), errors: { ...state.errors, [action.name]: action.error } };
  }
}

/** What the switch shows: the pending value while a flip is in flight, else the server's. */
export function effectiveEnabled(state: TogglesState, name: string, server: boolean): boolean {
  return state.overrides[name] ?? server;
}

const MAX_ROLE_BYTES = 63;

/** Why a run-as role can't be sent (null: it can). The API decides; this only catches the obvious. */
export function runAsReason(raw: string): string | null {
  const role = raw.trim();
  if (role === "") return "Enter a role name.";
  if (role.includes("\u0000")) return "A role name can't contain that character.";
  if (new TextEncoder().encode(role).length > MAX_ROLE_BYTES) return `A role name is at most ${MAX_ROLE_BYTES} bytes.`;
  if (actAsReserved(role)) return `A function can't run as ${role}: it's reserved.`;
  return null;
}

/** The API's rule for a secret's name: a letter or `_`, then up to 127 of A-Z, a-z, 0-9 and `_`. */
export function secretNameReason(raw: string): string | null {
  if (raw === "") return "Enter a name.";
  return /^[A-Za-z_][A-Za-z0-9_]{0,127}$/.test(raw) ? null : "Start with a letter or _, then up to 127 letters, digits or _.";
}

/** The URL a client calls: a public function names its branch; any other is routed by the caller's key. */
export function invokeUrl(apiUrl: string, auth: FunctionAuth | null, branchId: string, name: string): string {
  const base = apiUrl.replace(/\/+$/, "");
  const fn = encodeURIComponent(name);
  return auth === "public" ? `${base}/functions/v1/public/${branchId}/${fn}` : `${base}/functions/v1/${fn}`;
}

export function curlFor(apiUrl: string, auth: FunctionAuth | null, branchId: string, name: string): string {
  const url = invokeUrl(apiUrl, auth, branchId, name);
  return auth === "public" ? `curl -X POST ${url}` : `curl -X POST ${url} \\\n  -H "Authorization: Bearer $CHELABASE_DATA_KEY"`;
}

export function isPlatformModeRefusal(err: unknown): boolean {
  return err instanceof ApiError && err.code === "not_available_in_platform_mode";
}

/** The API's refusals, with the 409 reasons said plainly. */
export function functionErrorText(err: unknown): string {
  if (err instanceof ApiError && err.status === 409) {
    const reason = err.extra.reason;
    if (reason === "function_active_on_protected_branch") return "This is a protected branch: only an Admin can change an active function here.";
    if (reason === "managed_key") return "That key is a function's run-as key: change it through the function's run-as role.";
  }
  return errorText(err);
}
