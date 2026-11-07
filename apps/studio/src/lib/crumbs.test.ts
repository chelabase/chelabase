import { describe, expect, it } from "vitest";
import { TABLES_ROUTE } from "@/components/app-shell/rail-items";
import { buildCrumbs, type CrumbLabels } from "./crumbs";
import { NARROW_QUERY } from "./use-media-query";

const DB = "11111111-1111-4111-8111-111111111111";
const BR = "22222222-2222-4222-8222-222222222222";
const lab = (id: string, label: string) => ({ id, label });
const labels = (l: CrumbLabels = {}) => l;

const summary = (cs: ReturnType<typeof buildCrumbs>) => cs.map((c) => ({ label: c.label, link: c.to !== undefined, loading: !!c.loading }));

describe("NARROW_QUERY", () => {
  it("keeps the substring the existing mocks match on", () => {
    expect(NARROW_QUERY).toContain("width < 40rem");
  });
});

describe("buildCrumbs: shapes", () => {
  it("/app and /app/ give Projects, not a link", () => {
    for (const p of ["/app", "/app/"]) {
      expect(summary(buildCrumbs(p, {}, labels()))).toEqual([{ label: "Projects", link: false, loading: false }]);
    }
  });

  it("unknown paths give no crumbs", () => {
    for (const p of ["/", "/login", "/app/nope", "/application", ""]) {
      expect(buildCrumbs(p, {}, labels())).toEqual([]);
    }
  });

  it("a project page: Projects (link to /app) then the project label", () => {
    const cs = buildCrumbs("/app/projects/p1", { projectId: "p1" }, labels({ projectId: lab("p1", "Acme") }));
    expect(cs).toMatchObject([{ label: "Projects", to: "/app" }, { label: "Acme" }]);
    expect(cs[1].to).toBeUndefined();
    expect(cs[1].loading).toBeFalsy();
  });

  it("the API page is a single API crumb", () => {
    expect(summary(buildCrumbs("/app/api", {}, labels()))).toEqual([{ label: "API", link: false, loading: false }]);
  });

  it("the Data API page is a single Data API crumb at any place", () => {
    expect(summary(buildCrumbs("/app/data-api", {}, labels()))).toEqual([{ label: "Data API", link: false, loading: false }]);
    expect(summary(buildCrumbs("/app/storage/p1/d1/b1/avatars", { projectId: "p1", bucket: "avatars" }, labels()))).toEqual([{ label: "Storage", link: false, loading: false }]);
    expect(summary(buildCrumbs("/app/data-api/p1/d1/b1/app", { projectId: "p1", databaseId: "d1" }, labels()))).toEqual([{ label: "Data API", link: false, loading: false }]);
  });

  it("Members alone is a single crumb", () => {
    expect(summary(buildCrumbs("/app/members", {}, labels()))).toEqual([{ label: "Members", link: false, loading: false }]);
  });

  it("a member page: Members (plain /app/members, no search) then the name", () => {
    const cs = buildCrumbs("/app/members/m1", { memberId: "m1" }, labels({ memberId: lab("m1", "Ada") }));
    expect(cs).toMatchObject([{ label: "Members", to: "/app/members" }, { label: "Ada" }]);
    expect(cs[0].search).toBeUndefined();
    expect(cs[1].to).toBeUndefined();
  });

  it("a team page: Members > Teams (Teams tab) > team name", () => {
    const cs = buildCrumbs("/app/members/teams/t1", { teamId: "t1" }, labels({ teamId: lab("t1", "Billing") }));
    expect(cs).toMatchObject([{ label: "Members", to: "/app/members" }, { label: "Teams", to: "/app/members", search: { tab: "teams" } }, { label: "Billing" }]);
    expect(cs[2].to).toBeUndefined();
  });

  it("a team page without its label yet shows the loading mark", () => {
    const cs = buildCrumbs("/app/members/teams/t1", { teamId: "t1" }, labels({ teamId: lab("t0", "Other") }));
    expect(cs[2]).toMatchObject({ label: "…", loading: true });
  });

  it("/app/members/roles is the member page of a member whose id is 'roles' (params decide)", () => {
    const cs = buildCrumbs("/app/members/roles", { memberId: "roles" }, labels({ memberId: lab("roles", "Rolesy") }));
    expect(cs.map((c) => c.label)).toEqual(["Members", "Rolesy"]);
  });

  it("a role page: Members > Roles (Roles tab filtered to the branch) > role (S5)", () => {
    const cs = buildCrumbs(
      `/app/members/roles/${DB}/${BR}/app_user`,
      { databaseId: DB, branchId: BR, role: "app_user" },
      labels({ role: lab("app_user", "app_user") }),
    );
    expect(cs).toMatchObject([
      { label: "Members", to: "/app/members" },
      { label: "Roles", to: "/app/members", search: { tab: "roles", database: DB, branch: BR } },
      { label: "app_user" },
    ]);
    expect(cs[0].search).toBeUndefined();
    expect(cs[2].to).toBeUndefined();
  });

  it("the merge page: Projects > project (link, params) > database (text) > Merge", () => {
    const cs = buildCrumbs(
      "/app/projects/p1/databases/d1/merges/m1",
      { projectId: "p1", databaseId: "d1", mergeId: "m1" },
      labels({ projectId: lab("p1", "Acme"), databaseId: lab("d1", "main-db") }),
    );
    expect(cs).toMatchObject([
      { label: "Projects", to: "/app" },
      { label: "Acme", to: "/app/projects/$projectId", params: { projectId: "p1" } },
      { label: "main-db" },
      { label: "Merge" },
    ]);
    expect(cs[2].to).toBeUndefined(); // S4: plain text
    expect(cs[3].to).toBeUndefined();
    expect(cs[3].loading).toBeFalsy();
  });
});

describe("buildCrumbs: the Tables route (Q1, S6)", () => {
  it("no table chosen: a single Tables crumb, for /app/tables and with other params", () => {
    expect(summary(buildCrumbs("/app/tables", {}, labels()))).toEqual([{ label: "Tables", link: false, loading: false }]);
    expect(summary(buildCrumbs("/app/tables/p1/d1", { projectId: "p1", databaseId: "d1" }, labels()))).toEqual([
      { label: "Tables", link: false, loading: false },
    ]);
  });

  it("a table chosen: Tables links to the schema's list (params minus table), then the table label", () => {
    const params = { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "postgres", schema: "public", table: "users" };
    const cs = buildCrumbs("/app/tables/p1/d1/b1/postgres/public/users", params, labels({ table: lab("users", "users") }));
    expect(cs).toHaveLength(2);
    expect(cs[0].label).toBe("Tables");
    expect(cs[0].to).toBe(TABLES_ROUTE);
    expect(cs[0].params).toEqual({ projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "postgres", schema: "public" });
    expect(cs[0].params).not.toHaveProperty("table");
    expect(cs[1].label).toBe("users");
    expect(cs[1].to).toBeUndefined();
  });

  it("the optional segments may be missing: only the given params are passed on", () => {
    const cs = buildCrumbs("/app/tables/p1/d1/b1/postgres/public/users", { table: "users" }, labels({ table: lab("users", "users") }));
    expect(cs).toHaveLength(2);
    expect(cs[0].params ?? {}).toEqual({});
  });
});

describe("buildCrumbs: labels are keyed by id (S1) and loading shows '…' (P6)", () => {
  it("no label: '…' and loading, and not a link", () => {
    const cs = buildCrumbs("/app/projects/p1/databases/d1/merges/m1", { projectId: "p1", databaseId: "d1", mergeId: "m1" }, labels());
    expect(cs).toHaveLength(4);
    expect(cs[1]).toMatchObject({ label: "…", loading: true });
    expect(cs[1].to).toBeUndefined();
    expect(cs[2]).toMatchObject({ label: "…", loading: true });
    expect(cs[3].label).toBe("Merge");
  });

  it("a label for another id never shows (A's name painted over B)", () => {
    const cs = buildCrumbs("/app/projects/pB", { projectId: "pB" }, labels({ projectId: lab("pA", "Project A") }));
    expect(cs).toHaveLength(2);
    expect(cs[1]).toMatchObject({ label: "…", loading: true });
    expect(JSON.stringify(cs)).not.toContain("Project A");
  });

  it("an empty label or an empty id counts as no label", () => {
    for (const l of [lab("p1", ""), lab("", "X")]) {
      const cs = buildCrumbs("/app/projects/p1", { projectId: "p1" }, labels({ projectId: l }));
      expect(cs).toHaveLength(2);
      expect(cs[1]).toMatchObject({ loading: true });
    }
  });

  it("a loading project crumb in the middle of the merge trail is not a link", () => {
    const cs = buildCrumbs("/app/projects/p1/databases/d1/merges/m1", { projectId: "p1", databaseId: "d1", mergeId: "m1" }, labels({ databaseId: lab("d1", "db") }));
    expect(cs).toHaveLength(4);
    expect(cs[1].to).toBeUndefined();
    expect(cs[1].loading).toBe(true);
    expect(cs[2].label).toBe("db");
  });
});

describe("buildCrumbs: values come from params, not the pathname (S2)", () => {
  const role = "a/b%c é<x>";

  it("uses the router's decoded param for the label match and for link params", () => {
    // The pathname is only partly decoded by the router: it must not be parsed for values.
    const pathname = `/app/members/roles/${DB}/${BR}/a%2Fb%25c é<x>`;
    const cs = buildCrumbs(pathname, { databaseId: DB, branchId: BR, role }, labels({ role: lab(role, "shown") }));
    expect(cs).toHaveLength(3);
    expect(cs.map((c) => c.label)).toEqual(["Members", "Roles", "shown"]);
    expect(cs[1].search).toMatchObject({ database: DB, branch: BR });
  });

  it("project params in links are the raw decoded values (the router encodes them once)", () => {
    const id = "a/b%c é";
    const cs = buildCrumbs(
      `/app/projects/${encodeURIComponent(id)}/databases/d1/merges/m1`,
      { projectId: id, databaseId: "d1", mergeId: "m1" },
      labels({ projectId: lab(id, "P") }),
    );
    expect(cs).toHaveLength(4);
    expect(cs[1].params).toEqual({ projectId: id });
  });

  it("labels render as given, markup characters included (they are text, not HTML)", () => {
    const cs = buildCrumbs("/app/members/m1", { memberId: "m1" }, labels({ memberId: lab("m1", "<b>x</b>") }));
    expect(cs).toHaveLength(2);
    expect(cs[1].label).toBe("<b>x</b>");
  });
});

describe("buildCrumbs: keys", () => {
  it("every crumb has a distinct non-empty key", () => {
    const cs = buildCrumbs("/app/projects/p1/databases/d1/merges/m1", { projectId: "p1", databaseId: "d1", mergeId: "m1" }, labels());
    expect(cs).toHaveLength(4);
    const keys = cs.map((c) => c.key);
    expect(keys.every((k) => k)).toBe(true);
    expect(new Set(keys).size).toBe(keys.length);
  });
});
