import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Member } from "@chelabase/management-client";
import { SOFIA, calls, mount, sofiaFor, LOGIN_SOON } from "./test-setup";

/** Fix S2 (recapture at 390 px): the member page. Layout facts only jsdom can't see are checked in the recapture. */

const REINSTATE = `POST /v1/members/${SOFIA}/reinstate`;

describe("Reinstate (S2 finding 1)", () => {
  it("confirms_sends_the_request_drops_the_banner_shows_active_and_says_so_once", async () => {
    let status: Member["status"] = "suspended";
    const m = mount({
      caller: "tenant_admin",
      members: () => [sofiaFor("tenant_admin", { status })],
      routes: { [REINSTATE]: () => (status = "active", Response.json(sofiaFor("tenant_admin"))) },
    });
    const header = (await screen.findByRole("heading", { level: 1, name: /Sofia Marchetti/ })).closest("header")!;
    await m.user.click(await screen.findByRole("button", { name: "Reinstate" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Reinstate Sofia Marchetti?" });
    await m.user.click(within(dialog).getByRole("button", { name: "Reinstate" }));
    await waitFor(() => expect(calls(m.f, "POST", "/v1/members")).toHaveLength(1));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    await waitFor(() => expect(screen.queryByText(/can't sign in to Studio/)).toBeNull());
    expect(screen.queryByText(/^Suspended on /)).toBeNull();
    expect(header).toHaveTextContent("Active");
    expect(header).not.toHaveTextContent("Suspended");
    expect(await screen.findByRole("status")).toHaveTextContent(/Sofia Marchetti is reinstated/);
    expect(screen.getByRole("button", { name: "Suspend" })).toBeInTheDocument();
  });
});

describe("The header keeps the status (S2 finding 2)", () => {
  it("a_suspended_member_s_header_says_Suspended_beside_the_banner", async () => {
    mount({ caller: "tenant_admin", members: () => [sofiaFor("tenant_admin", { status: "suspended" })] });
    const header = (await screen.findByRole("heading", { level: 1, name: /Sofia Marchetti/ })).closest("header")!;
    await screen.findByText(/^Suspended on /);
    expect(header).toHaveTextContent("Suspended");
  });

  it("an_active_member_s_header_says_Active", async () => {
    mount({ caller: "tenant_admin" });
    const header = (await screen.findByRole("heading", { level: 1, name: /Sofia Marchetti/ })).closest("header")!;
    expect(header).toHaveTextContent("Active");
  });
});

describe("Narrow screens (S2 findings 3 and 4)", () => {
  it("the_logins_table_has_a_minimum_width_so_its_frame_scrolls_and_the_actions_stay_reachable", async () => {
    mount({ caller: "self", memberId: SOFIA, members: () => [sofiaFor("self")], logins: () => [LOGIN_SOON] });
    const table = await screen.findByRole("table", { name: "Personal Postgres logins" });
    expect(table.className).toMatch(/(^|\s)min-w-\[\d/);
    expect(table.parentElement?.className).toMatch(/overflow-x-auto/);
    // The menu button is in the table, inside that frame.
    expect(within(table).getByRole("button", { name: /More actions for sofia on billing-prod/ })).toBeInTheDocument();
  });

  it("the_header_email_can_break_after_the_at_sign_and_dots", async () => {
    mount({ caller: "tenant_admin" });
    const header = (await screen.findByRole("heading", { level: 1, name: /Sofia Marchetti/ })).closest("header")!;
    const email = within(header).getByText((_, el) => el?.tagName === "SPAN" && el.textContent === "sofia@northwind.dev");
    expect(email.querySelectorAll("wbr").length).toBeGreaterThanOrEqual(2);
  });
});
