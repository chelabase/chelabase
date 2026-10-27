import { describe, expect, it } from "vitest";
import { ApiError } from "@chelabase/management-client";
import { refusalExtra, refusalMessage, refusalOrErrorText, refusalSentence } from "./refusal-extra";

const err = (extra: Record<string, unknown>) => new ApiError(422, "invalid_request", "refused", extra);

describe("refusalExtra", () => {
  it("reads the role, chain, database, project and branch of a refusal", () => {
    expect(
      refusalExtra(
        err({ role: "reports", chain: ["reports", "svc"], database_id: "d1", project_id: "p1", branch: "dev", branch_main: false }),
      ),
    ).toEqual({ role: "reports", chain: ["reports", "svc"], databaseId: "d1", projectId: "p1", branch: "dev", branchMain: false });
  });

  it("ignores what is the wrong shape, and an empty chain", () => {
    expect(refusalExtra(err({ role: 3, chain: [], database_id: null, branch_main: "no", why: "nonsense" }))).toEqual({
      role: undefined, chain: undefined, databaseId: undefined, projectId: undefined, branch: undefined, branchMain: undefined, why: undefined,
    });
    expect(refusalExtra(err({ chain: ["a", 4, "b"] })).chain).toEqual(["a", "b"]);
    expect(refusalExtra(err({ why: "login" })).why).toBe("login");
  });
});

describe("refusalMessage", () => {
  // The browser pass: Studio said the server's sentence verbatim (backticks, "through reporting
  // it reaches `reporting`"); it now says it in its own words from `extra`.
  it("says who may hold the role, where, and why", () => {
    const base = { role: "reporting", chain: ["reporting"], why: "login" as const };
    expect(refusalMessage({ ...base }, "app_main")).toBe("Only Admin can hold reporting on app_main: it's a LOGIN role.");
    expect(refusalMessage({ role: "ops", chain: ["ops", "writer"], why: "login" }, "app_main")).toBe(
      "Only Admin can hold ops on app_main: it reaches writer, a LOGIN role.",
    );
    expect(refusalMessage({ role: "lead", chain: ["lead"], why: "admin" }, "web")).toBe(
      "Only Admin can hold lead on web: it holds ADMIN on another role.",
    );
    expect(refusalMessage({ role: "sneaky", chain: ["sneaky", "postgres"], why: "owner_level" }, "web")).toBe(
      "Only Admin can hold sneaky on web: it reaches postgres, which is owner-level.",
    );
    expect(refusalMessage({ role: "x", chain: ["x", "pg_y"], why: "reserved" }, "web", "dev", false)).toBe(
      "Only Admin can hold x on web, branch dev: it reaches pg_y, which is reserved.",
    );
    expect(refusalMessage({ role: "a", chain: ["a", "b"], why: "admin" }, "web")).toBe(
      "Only Admin can hold a on web: it reaches b, which holds ADMIN on another role.",
    );
  });

  it("is null when the refusal doesn't say enough", () => {
    expect(refusalMessage({ role: "r", chain: ["r"] }, "db")).toBeNull();
    expect(refusalMessage({ chain: ["r"], why: "login" }, "db")).toBeNull();
    expect(refusalMessage({ role: "r", why: "login" }, "db")).toBeNull();
  });
});

describe("refusalSentence", () => {
  const server =
    "`app_login` can be held only with the admin platform role: on database `ledger` the member would be developer with it, and `app_login` is a LOGIN role";
  const refused = (extra: Record<string, unknown> = {}) => new ApiError(422, "invalid_request", server, { role: "app_login", ...extra });

  it("capitalises_levels_and_names_the_team", () => {
    const text = refusalSentence(refused({ team_id: "t1" }), { teamName: (id) => (id === "t1" ? "Ops" : undefined) });
    expect(text).toBe(
      "`app_login` can be held only with the Admin platform role: on database `ledger` the member would be Developer with it, and `app_login` is a LOGIN role. The Ops team gives that access.",
    );
  });

  it("names_no_team_when_the_id_is_unknown_or_absent", () => {
    expect(refusalSentence(refused({ team_id: "gone" }), { teamName: () => undefined })).not.toMatch(/team gives/);
    expect(refusalSentence(refused())).not.toMatch(/team gives/);
  });

  it("reads_the_team_id_extra", () => {
    expect(refusalExtra(refused({ team_id: "t1" })).teamId).toBe("t1");
  });

  it("refusalOrErrorText_leaves_other_errors_alone", () => {
    expect(refusalOrErrorText(new ApiError(409, "conflict", "changed since"))).toBe("changed since");
    expect(refusalOrErrorText(refused())).toContain("Admin platform role");
  });
});
