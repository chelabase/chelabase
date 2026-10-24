import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import type { BranchRole } from "@chelabase/management-client";
import { passwordlessString } from "./logins";
import {
  B2, CONNECTION, D2, LOGIN_LATER, LOGIN_SOON, P2, PASSWORD, SOFIA, calls, meFor, mount, ok, secret, sofiaFor, type Opts,
} from "./test-setup";

/** Final fix round, area C: EOS-2 (verify-full), EOS-3 (the default-role picker's safety rule), EOS-H3 (where the secret lives). */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const RESET_D2 = `POST /v1/databases/${D2}/personal-login/reset-password`;
const role = (name: string, extra: Partial<BranchRole> = {}) =>
  ({
    name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false, guard_exempt: false,
    ...extra,
  }) as BranchRole;

function self(opts: Opts = {}) {
  return mount({ caller: "self", memberId: SOFIA, members: () => [sofiaFor("self")], logins: () => [LOGIN_SOON, LOGIN_LATER], ...opts });
}

/** Sofia's `/v1/me` with her Data warehouse access replaced. */
function meWith(project: { platform_role: string; roles: string[] | "all" }) {
  const me = meFor("self") as unknown as { projects: { project_id: string; platform_role: string; roles: unknown; databases: Record<string, unknown>[] }[] };
  return {
    ...me,
    projects: me.projects.map((p) =>
      p.project_id === P2 ? { ...p, ...project, databases: p.databases.map((d) => ({ ...d, ...project })) } : p,
    ),
  };
}

async function openReveal(opts: Opts) {
  const m = self({ ...opts, routes: { [RESET_D2]: ok(secret(LOGIN_LATER, { connection_string: CONNECTION })), ...opts.routes } });
  const row = await screen.findByRole("row", { name: /sofia_m/ });
  await m.user.click(within(row).getByRole("button", { name: "Reset password" }));
  const dialog = await screen.findByRole("dialog", { name: "Your login sofia_m on warehouse" });
  return { ...m, dialog };
}
const optionsOf = (select: HTMLElement) => within(select).getAllByRole("option").map((o) => o.textContent);

const WAREHOUSE_ROLES = [
  role("wh_owner", { origin: "owner", level: "owner_level", login: true }),
  role("reports_reader"),
  role("etl_writer"),
  role("svc_login", { login: true }),
  role("wh_super", { level: "owner_level" }),
  role("bypasser", { bypass_rls: true, level: "owner_level" }),
  role("via_login", { member_of: ["svc_login"] }),
];
const ROLES_ROUTE = { [`GET /v1/databases/${D2}/branches/${B2}/roles`]: ok(WAREHOUSE_ROLES) };

describe("the passwordless connection string (EOS-2)", () => {
  it("verifies_the_server_certificate_like_the_server_s_own_string", () => {
    const db = { host: "h", port: 5432, postgres_database: "warehouse" };
    const s = passwordlessString({ role_name: "sofia_m" }, db);
    expect(s).toBe("postgresql://sofia_m@h:5432/warehouse?sslmode=verify-full");
    expect(s).not.toContain("sslmode=require");
  });
});

describe("the default-role picker offers only roles a personal login can hold (EOS-3)", () => {
  it("an_admin_is_not_offered_owner_level_login_or_login_reaching_roles", async () => {
    const { dialog } = await openReveal({ routes: { "GET /v1/me": ok(meWith({ platform_role: "admin", roles: "all" })), ...ROLES_ROUTE } });
    const picker = within(dialog).getByLabelText("Default role");
    // Never the owner, a LOGIN role, an owner-level (or BYPASSRLS) role, or one that reaches a LOGIN role.
    expect(optionsOf(picker).sort()).toEqual(["None", "etl_writer", "reports_reader"]);
  });

  it("a_developer_is_not_offered_a_held_role_that_is_login_or_owner_level", async () => {
    const { dialog } = await openReveal({
      routes: {
        "GET /v1/me": ok(meWith({ platform_role: "developer", roles: ["etl_writer", "svc_login", "wh_super", "via_login"] })),
        ...ROLES_ROUTE,
      },
    });
    const picker = await within(dialog).findByLabelText("Default role");
    // Wait for the branch's roles to settle before judging the list.
    await within(dialog).findByText(/Your roles here/);
    expect(optionsOf(picker).sort()).toEqual(["None", "etl_writer"]);
  });

  it("a_viewer_s_roles_are_not_offered_below_developer", async () => {
    const { dialog } = await openReveal({
      routes: { "GET /v1/me": ok(meWith({ platform_role: "viewer", roles: ["etl_writer", "reports_reader"] })), ...ROLES_ROUTE },
    });
    await within(dialog).findByText(/Your roles here/);
    expect(within(dialog).queryByLabelText("Default role")).toBeNull();
    expect(within(dialog).getByText(/Your roles here/)).toHaveTextContent("none yet");
  });

  it("a_developer_s_plain_roles_are_still_offered_and_chosen_into_the_string", async () => {
    const { dialog, user } = await openReveal({
      routes: { "GET /v1/me": ok(meWith({ platform_role: "developer", roles: ["reports_reader", "etl_writer"] })), ...ROLES_ROUTE },
    });
    const picker = await within(dialog).findByLabelText("Default role");
    expect(optionsOf(picker).sort()).toEqual(["None", "etl_writer", "reports_reader"]);
    await user.selectOptions(picker, "etl_writer");
    expect(within(dialog).getByLabelText("Connection string · main branch")).toHaveValue(`${CONNECTION}&options=-c%20role%3Detl_writer`);
  });
});

// ---- EOS-H3 ----

interface Fiber {
  type: unknown;
  child: Fiber | null;
  sibling: Fiber | null;
  memoizedState: unknown;
  stateNode: unknown;
}

/** The current fiber tree of a React root. */
function rootFiber(container: HTMLElement): Fiber {
  const key = Object.keys(container).find((k) => k.startsWith("__reactContainer$"));
  if (!key) throw new Error("no React root on the container");
  const host = (container as unknown as Record<string, Fiber>)[key];
  return (host.stateNode as { current: Fiber }).current;
}
const nameOf = (f: Fiber) => (typeof f.type === "function" ? (f.type as { displayName?: string; name: string }).displayName ?? (f.type as { name: string }).name : null);

/** Whether a value (a hook's state: a string, an object, a ref) holds `needle` anywhere, with a depth and cycle guard. */
function holds(value: unknown, needle: string, seen = new WeakSet<object>(), depth = 0): boolean {
  if (typeof value === "string") return value.includes(needle);
  if (value === null || typeof value !== "object" || depth > 5 || seen.has(value)) return false;
  if (value instanceof Node || (value as { $$typeof?: unknown }).$$typeof !== undefined) return false;
  seen.add(value);
  return Object.values(value as Record<string, unknown>).some((v) => holds(v, needle, seen, depth + 1));
}

/** The names of the components, outside `RevealDialog`'s subtree, whose hook state holds the text. */
function stateHolders(container: HTMLElement, needle: string): string[] {
  const out: string[] = [];
  const walk = (f: Fiber | null) => {
    for (let n = f; n; n = n.sibling) {
      if (nameOf(n) === "RevealDialog") continue; // its own state is where the secret belongs
      if (typeof n.type === "function") {
        for (let h = n.memoizedState as { memoizedState: unknown; next: unknown } | null; h && typeof h === "object"; h = h.next as typeof h) {
          if (holds(h.memoizedState, needle)) {
            out.push(nameOf(n) ?? "anonymous");
            break;
          }
        }
      }
      walk(n.child);
    }
  };
  walk(rootFiber(container));
  return out;
}

describe("the password lives only in the reveal dialog's own state (EOS-H3)", () => {
  it("a_reset_password_is_held_by_no_component_outside_the_dialog", async () => {
    const m = await openReveal({});
    expect(within(m.dialog).getByLabelText("Password")).toHaveValue(PASSWORD);
    // The page (PersonalLogins and everything above) must not keep it in state.
    expect(stateHolders(m.container, PASSWORD)).toEqual([]);
  });

  it("a_created_login_s_password_is_held_by_no_component_outside_the_dialog", async () => {
    const created = secret({ ...LOGIN_LATER, role_name: "sofia_w" });
    const m = self({ routes: { [`PUT /v1/databases/${D2}/personal-login`]: ok(created, 201) } });
    await m.user.click(await screen.findByRole("button", { name: "New login" }));
    const newLogin = await screen.findByRole("dialog", { name: "New login" });
    await m.user.selectOptions(within(newLogin).getByLabelText("Project"), P2);
    await m.user.click(within(newLogin).getByRole("button", { name: "Create login" }));
    const dialog = await screen.findByRole("dialog", { name: /Your login/ });
    expect(within(dialog).getByLabelText("Password")).toHaveValue(PASSWORD);
    expect(calls(m.f, "PUT", "/v1/databases")).toHaveLength(1);
    expect(stateHolders(m.container, PASSWORD)).toEqual([]);
  });

  it("closing_the_dialog_leaves_the_password_nowhere_in_state", async () => {
    const m = await openReveal({});
    await m.user.click(within(m.dialog).getByRole("button", { name: "Done" }));
    await screen.findByRole("table", { name: "Personal Postgres logins" });
    expect(screen.queryByRole("dialog", { name: /Your login/ })).toBeNull();
    expect(stateHolders(m.container, PASSWORD)).toEqual([]);
  });
});
