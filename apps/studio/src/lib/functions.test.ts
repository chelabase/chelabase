import { ApiError } from "@chelabase/management-client";
import { describe, expect, it } from "vitest";
import {
  curlFor,
  effectiveEnabled,
  functionErrorText,
  initialToggles,
  invokeUrl,
  isPlatformModeRefusal,
  runAsReason,
  secretNameReason,
  togglesReducer,
} from "./functions";

const B1 = "11111111-1111-1111-1111-111111111111";
const B2 = "22222222-2222-2222-2222-222222222222";

describe("togglesReducer", () => {
  it("flip_shows_the_new_value_at_once", () => {
    const s = togglesReducer(initialToggles(B1), { type: "flip", scope: B1, name: "hello", enabled: true });
    expect(effectiveEnabled(s, "hello", false)).toBe(true);
    // Others follow the server.
    expect(effectiveEnabled(s, "other", false)).toBe(false);
  });

  it("success_drops_the_override_and_keeps_no_error", () => {
    let s = togglesReducer(initialToggles(B1), { type: "flip", scope: B1, name: "hello", enabled: true });
    s = togglesReducer(s, { type: "settled", scope: B1, name: "hello" });
    expect(s.overrides).toEqual({});
    expect(s.errors).toEqual({});
    expect(effectiveEnabled(s, "hello", true)).toBe(true);
  });

  it("failure_reverts_to_the_server_value_and_keeps_the_error", () => {
    let s = togglesReducer(initialToggles(B1), { type: "flip", scope: B1, name: "hello", enabled: true });
    s = togglesReducer(s, { type: "failed", scope: B1, name: "hello", error: "needs admin" });
    expect(effectiveEnabled(s, "hello", false)).toBe(false);
    expect(s.errors.hello).toBe("needs admin");
  });

  it("a_new_flip_clears_that_functions_earlier_error", () => {
    let s = togglesReducer(initialToggles(B1), { type: "failed", scope: B1, name: "hello", error: "boom" });
    s = togglesReducer(s, { type: "flip", scope: B1, name: "hello", enabled: false });
    expect(s.errors.hello).toBeUndefined();
  });

  it("branch_change_resets_every_override_and_error", () => {
    let s = togglesReducer(initialToggles(B1), { type: "flip", scope: B1, name: "hello", enabled: true });
    s = togglesReducer(s, { type: "failed", scope: B1, name: "other", error: "boom" });
    s = togglesReducer(s, { type: "scope", scope: B2 });
    expect(s).toEqual(initialToggles(B2));
    expect(effectiveEnabled(s, "hello", false)).toBe(false);
  });

  it("answers_from_the_previous_branch_are_ignored", () => {
    let s = togglesReducer(initialToggles(B1), { type: "flip", scope: B1, name: "hello", enabled: true });
    s = togglesReducer(s, { type: "scope", scope: B2 });
    const after = togglesReducer(s, { type: "failed", scope: B1, name: "hello", error: "late" });
    expect(after).toBe(s);
    expect(togglesReducer(s, { type: "flip", scope: B1, name: "x", enabled: true })).toBe(s);
  });

  it("the_same_scope_keeps_the_state", () => {
    const s = togglesReducer(initialToggles(B1), { type: "flip", scope: B1, name: "hello", enabled: true });
    expect(togglesReducer(s, { type: "scope", scope: B1 })).toBe(s);
  });
});

describe("runAsReason", () => {
  it("accepts_a_role_name", () => {
    expect(runAsReason("reporter")).toBeNull();
    expect(runAsReason("  app_writer ")).toBeNull();
    expect(runAsReason("a".repeat(63))).toBeNull();
  });

  it("refuses_empty_too_long_and_nul", () => {
    expect(runAsReason("")).toMatch(/Enter a role/);
    expect(runAsReason("   ")).toMatch(/Enter a role/);
    expect(runAsReason("a".repeat(64))).toMatch(/63/);
    // Bytes, not characters: Postgres caps identifiers at 63 bytes.
    expect(runAsReason("é".repeat(32))).toMatch(/63/);
    expect(runAsReason("a\u0000b")).toMatch(/character/);
  });

  it("refuses_roles_no_function_may_run_as", () => {
    expect(runAsReason("owner")).toMatch(/can't run as/);
    expect(runAsReason("pg_read_all_data")).toMatch(/can't run as/);
    expect(runAsReason("postgres")).toMatch(/can't run as/);
  });
});

describe("secretNameReason", () => {
  it("follows_the_api_rule", () => {
    expect(secretNameReason("STRIPE_KEY")).toBeNull();
    expect(secretNameReason("_x1")).toBeNull();
    expect(secretNameReason("A" + "b".repeat(127))).toBeNull();
    expect(secretNameReason("")).toMatch(/Enter a name/);
    expect(secretNameReason("1ABC")).toMatch(/letter or _/);
    expect(secretNameReason("A-B")).toMatch(/letter or _/);
    expect(secretNameReason("A" + "b".repeat(128))).toMatch(/letter or _/);
  });
});

describe("invoke URLs", () => {
  it("public_form_names_the_branch_key_form_does_not", () => {
    expect(invokeUrl("http://api.test/", "public", B1, "hello")).toBe(`http://api.test/functions/v1/public/${B1}/hello`);
    expect(invokeUrl("http://api.test", "key", B1, "hello")).toBe("http://api.test/functions/v1/hello");
    expect(invokeUrl("http://api.test", "app_user", B1, "hello")).toBe("http://api.test/functions/v1/hello");
    expect(invokeUrl("http://api.test", null, B1, "a b")).toBe("http://api.test/functions/v1/a%20b");
  });

  it("curl_sends_the_key_only_when_one_is_needed", () => {
    expect(curlFor("http://api.test", "public", B1, "hello")).toBe(`curl -X POST http://api.test/functions/v1/public/${B1}/hello`);
    expect(curlFor("http://api.test", "key", B1, "hello")).toBe(
      'curl -X POST http://api.test/functions/v1/hello \\\n  -H "Authorization: Bearer $CHELABASE_DATA_KEY"',
    );
  });
});

describe("errors", () => {
  it("platform_mode_refusal_is_recognised", () => {
    expect(isPlatformModeRefusal(new ApiError(403, "not_available_in_platform_mode", "no"))).toBe(true);
    expect(isPlatformModeRefusal(new ApiError(403, "forbidden", "no"))).toBe(false);
    expect(isPlatformModeRefusal(new Error("x"))).toBe(false);
  });

  it("explains_the_409_reasons", () => {
    expect(functionErrorText(new ApiError(409, "conflict", "raw", { reason: "function_active_on_protected_branch" }))).toMatch(/protected branch/);
    expect(functionErrorText(new ApiError(409, "conflict", "raw", { reason: "managed_key" }))).toMatch(/run-as key/);
    expect(functionErrorText(new ApiError(422, "invalid_request", "role \"x\" does not exist"))).toBe('role "x" does not exist');
  });
});
