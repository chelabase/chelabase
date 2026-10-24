import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { BranchRole, ProjectGrant } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { GrantCard } from "./index";
import { P1, person } from "./fixtures";

function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false,
    ...extra,
  } as BranchRole;
}

const sources = [{ database: "orders", roles: [role("postgres", { origin: "owner", level: "owner_level", login: true }), role("billing"), role("reports")] }];
const grant: ProjectGrant = { member_id: "bo", platform_role: "developer", pg_roles: ["billing"], overrides: [] };
const bo = person("bo", { level: "developer", projects: [[P1, "developer"]] });

function setup(handler: FakeHandler, g: ProjectGrant = grant) {
  const f = fakeFetch({ [`PUT /v1/projects/${P1}/grants/bo`]: handler });
  const onSaved = vi.fn();
  renderWithQuery(<GrantCard projectId={P1} member={bo} grant={g} roleSources={sources} onSaved={onSaved} />, { fetch: f });
  return { f, onSaved };
}
const puts = (f: { calls: Request[] }) => f.calls.filter((r) => r.method === "PUT");

describe("GrantCard", () => {
  it("grant_card_sends_the_own_grant_only", async () => {
    const { f, onSaved } = setup(() => jsonResponse({ ...grant, pg_roles: ["billing", "reports"] }));
    // Nothing changed: nothing to save, so no Save at all (fix S).
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    await userEvent.click(screen.getByRole("checkbox", { name: "reports" }));
    const save = screen.getByRole("button", { name: "Save" });
    expect(save).toBeEnabled();
    await userEvent.click(save);
    await waitFor(() => expect(puts(f)).toHaveLength(1));
    const body = await puts(f)[0].clone().json();
    // Exactly the own grant: a platform role and the checked roles, nothing else.
    expect(Object.keys(body).sort()).toEqual(["pg_roles", "platform_role"]);
    expect(body.platform_role).toBe("developer");
    expect([...body.pg_roles].sort()).toEqual(["billing", "reports"]);
    await waitFor(() => expect(onSaved).toHaveBeenCalledTimes(1));
  });

  it("sends no roles for Admin and never lists the owner as a choice", async () => {
    const { f } = setup(() => jsonResponse({ ...grant, platform_role: "admin", pg_roles: "all" }));
    expect(screen.queryByRole("checkbox", { name: "postgres" })).toBeNull();
    await userEvent.click(screen.getByRole("radio", { name: /^Admin/ }));
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(puts(f)).toHaveLength(1));
    expect(await puts(f)[0].clone().json()).toEqual({ platform_role: "admin", pg_roles: [] });
  });

  it("shows a refusal on the card and does not report a save", async () => {
    const { f, onSaved } = setup(() => jsonResponse({ error: { code: "invalid_request", message: "billing reaches an owner-level role" } }, 422));
    await userEvent.click(screen.getByRole("checkbox", { name: "reports" }));
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(puts(f)).toHaveLength(1));
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(onSaved).not.toHaveBeenCalled();
    // Still editable afterwards.
    expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();
  });
});
