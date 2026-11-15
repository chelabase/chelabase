import { useEffect, useReducer } from "react";
import { ApiError, type Function as FunctionInfo, type FunctionAuth, type FunctionKind, type ServiceRollout } from "@chelabase/management-client";
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

/**
 * The URL a client calls. A service is at `/services/v1/{name}` with a data key; a public
 * function names its branch; any other function is routed by the caller's key.
 */
export function invokeUrl(apiUrl: string, auth: FunctionAuth | null, branchId: string, name: string, kind: FunctionKind = "function"): string {
  const base = apiUrl.replace(/\/+$/, "");
  const fn = encodeURIComponent(name);
  if (kind === "service") return `${base}/services/v1/${fn}`;
  return auth === "public" ? `${base}/functions/v1/public/${branchId}/${fn}` : `${base}/functions/v1/${fn}`;
}

const KEY_HEADER = '-H "Authorization: Bearer $CHELABASE_DATA_KEY"';

export function curlFor(apiUrl: string, auth: FunctionAuth | null, branchId: string, name: string, kind: FunctionKind = "function"): string {
  const url = invokeUrl(apiUrl, auth, branchId, name, kind);
  if (kind === "service") return `curl ${url} \\\n  ${KEY_HEADER}`;
  return auth === "public" ? `curl -X POST ${url}` : `curl -X POST ${url} \\\n  ${KEY_HEADER}`;
}

/**
 * A container service's rollover, said plainly: pending while the new container is started and
 * checked (the previous deployment serves meanwhile), failed with the reason. `null` when none.
 */
export function rolloutNotice(rollout: ServiceRollout | null): { tone: "pending" | "failed"; text: string } | null {
  if (!rollout) return null;
  const id = rollout.deployment_id.slice(0, 8);
  if (rollout.state === "pending") {
    return { tone: "pending", text: `Rolling over to ${id}: waiting for its container to pass its health check. The previous deployment serves meanwhile.` };
  }
  return { tone: "failed", text: `The rollover to ${id} failed, so the previous deployment still serves: ${rollout.error ?? "no reason given"}` };
}

/** Whether any function's rollover is pending (the list polls until none is). */
export function rolloutPending(functions: Pick<FunctionInfo, "rollout">[]): boolean {
  return functions.some((f) => f.rollout?.state === "pending");
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

/**
 * Optimistic switches for one scope (a function's schedules, say): `flip` shows the new value at
 * once, sends it, refreshes, and reverts with the reason on a refusal. A new scope starts empty.
 */
export function useToggles(scope: string) {
  const [state, dispatch] = useReducer(togglesReducer, scope, initialToggles);
  useEffect(() => dispatch({ type: "scope", scope }), [scope]);
  async function flip(id: string, enabled: boolean, send: () => Promise<unknown>, refresh: () => Promise<unknown>) {
    dispatch({ type: "flip", scope, name: id, enabled });
    try {
      await send();
      await refresh();
      dispatch({ type: "settled", scope, name: id });
    } catch (err) {
      dispatch({ type: "failed", scope, name: id, error: functionErrorText(err) });
    }
  }
  return {
    enabled: (id: string, server: boolean) => effectiveEnabled(state, id, server),
    pending: (id: string) => id in state.overrides,
    error: (id: string) => state.errors[id],
    flip,
  };
}
