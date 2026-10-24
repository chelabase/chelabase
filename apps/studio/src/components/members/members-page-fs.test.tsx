import { useState } from "react";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { MeResponse as Me, Member } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { expectThemedSelects } from "@/test/themed";
import { MembersPage, type MembersTab } from "./members-page";
import { person } from "./hub/fixtures";

/** Fix S: the page-level status after an invite (fix 1) and the toolbars' themed controls (fix 7). */

const adminMe = {
  sub: "u1", email: "ana.personal@elsewhere.test", name: "Ana", tenant_id: "t1",
  member: { id: "ana", status: "active", tenant_admin: true }, instance_admin: false, tenant_admins: [],
  projects: [{ project_id: "p1", platform_role: "admin", roles: "all", databases: [] }],
  user: { sub: "u1", org_id: "org-1" },
  tenant: { id: "t1", slug: "demo", display_name: null, projects: [{ id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] }] },
} as unknown as Me;

const people: Member[] = [person("ana", { name: "Ana", level: "tenant_admin" }), person("bo", { name: "Bo", level: "viewer", projects: [["11111111-1111-4111-8111-111111111111", "viewer"]] })];

const ok = (body: unknown, status = 200): FakeHandler => () => jsonResponse(body, status);

function setup() {
  const f = fakeFetch({
    "GET /v1/me": ok(adminMe),
    "GET /v1/members": ok(people),
    "GET /v1/invites/delivery": ok({ email_available: true, reason: null }),
    "GET /v1/access-events": ok({ events: [], next_before: null }),
    "POST /v1/invites": ok({ outcome: "invited", member: { ...person("new", { status: "invited" }), email: "new@example.test" } }, 201),
  });
  function Harness() {
    const [tab, setTab] = useState<MembersTab>("people");
    return <MembersPage tab={tab} onTabChange={setTab} />;
  }
  return renderWithQuery(<Harness />, { fetch: f });
}

async function invite() {
  await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite people" });
  await userEvent.type(within(dialog).getByLabelText("Emails"), "new@example.test");
  await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
}

describe("the post-invite status (fix 1)", () => {
  it("is_a_live_region_that_does_not_sit_in_the_page_flow_above_the_tabs", async () => {
    setup();
    await invite();
    const status = await screen.findByRole("status");
    expect(status).toHaveTextContent("Invited new@example.test.");
    // Overlaid (a toast-like layer): nothing in the flow moves the tabs down when it appears.
    expect(status.closest(".fixed, .absolute")).not.toBeNull();
  });

  it("goes_away_by_itself", { timeout: 20_000 }, async () => {
    setup();
    await invite();
    await screen.findByRole("status");
    await waitFor(() => expect(screen.queryByRole("status")).toBeNull(), { timeout: 12_000 });
    // The tabs are still there and nothing else was lost.
    expect(screen.getByRole("tablist")).toBeInTheDocument();
  });
});

describe("themed controls on the people toolbar (fix 7)", () => {
  it("the_project_select_is_themed", async () => {
    const { container } = setup();
    await screen.findByRole("button", { name: "Invite people" });
    expectThemedSelects(container);
  });
});
