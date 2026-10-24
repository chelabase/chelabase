import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { BIDI_ESCAPED, CHANGES, PRIV, ROLES_URL, apiError, calls, mountPage, mountTab, privData } from "./test-setup";

/** Edge cases beyond the RED set (coder's own, task S3). */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const roleLink = (name: string) => screen.findByRole("link", { name: new RegExp(`^${name}\\b`) });

describe("a role card's privilege summary", () => {
  it("summarises_what_the_role_holds", async () => {
    localStorage.setItem("chelabase.members.view.roles", "cards");
    mountTab();
    const card = await roleLink("billing_editor");
    await waitFor(() => expect(card).toHaveTextContent("SELECT, INSERT on 4 tables in billing"));
    expect(card).toHaveTextContent("EXECUTE on 1 function in billing");
  });

  it("says_no_privileges_yet_or_names_its_memberships_when_it_holds_nothing", async () => {
    localStorage.setItem("chelabase.members.view.roles", "cards");
    const data = privData();
    for (const list of Object.values(data.objects)) for (const o of list) Object.assign(o, { grants: [], columns: o.columns.map((c) => ({ ...c, grants: [] })) });
    mountTab({ data });
    const bidi = await screen.findByRole("link", { name: (n) => n.startsWith(BIDI_ESCAPED) });
    await waitFor(() => expect(bidi).toHaveTextContent("No privileges yet"));
    await waitFor(() => expect(screen.getByRole("link", { name: /^billing_admin\b/ })).toHaveTextContent("Member of billing_editor"));
  });

  it("is_in_the_list_view_too_and_says_when_it_cant_be_read", async () => {
    mountTab({ routes: { [`GET ${PRIV}`]: apiError(500, "internal", "boom") } });
    const link = await roleLink("support_agent");
    await waitFor(() => expect(link).toHaveTextContent("Privileges couldn't be read"));
    // The list itself still loads: a summary's failure is not the tab's alert.
    expect(screen.queryByRole("alert")).toBeNull();
  });
});

describe("create role refuses before asking", () => {
  it.each([
    ["anon", /reserved/],
    ["chelabase_x", /reserved/],
    ["billing_reader", /already exists/],
  ])("a_name_like_%s", async (name, why) => {
    const m = mountTab();
    await roleLink("billing_editor");
    await m.user.click(screen.getByRole("button", { name: "Create role" }));
    const dialog = await screen.findByRole("dialog", { name: "Create a role" });
    await m.user.type(within(dialog).getByLabelText("Name"), name);
    expect(within(dialog).getByText(why)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Create role" })).toBeDisabled();
    expect(calls(m.f, "POST", ROLES_URL)).toHaveLength(0);
  });
});

describe("the role page", () => {
  const box = (label: string) => screen.getByRole("checkbox", { name: label });
  const loaded = async (opts: Parameters<typeof mountPage>[0] = {}) => {
    const m = mountPage(opts);
    await screen.findByRole("table", { name: "Tables and views in billing" });
    return m;
  };

  it("locks_the_matrix_while_a_review_is_open_and_unlocks_it_on_back", async () => {
    const m = await loaded();
    await m.user.click(box("DELETE on customers"));
    await m.user.click(screen.getByRole("button", { name: "Review changes" }));
    const panel = await screen.findByRole("complementary", { name: "Review changes" });
    await within(panel).findByRole("button", { name: /^Apply 1 change/ });
    expect(box("TRUNCATE on invoices")).toBeDisabled();
    expect(screen.getByRole("switch", { name: "All rows on refunds" })).toBeDisabled();
    await m.user.click(within(panel).getByRole("button", { name: "Back" }));
    expect(box("TRUNCATE on invoices")).toBeEnabled();
  });

  it("keep_grant_option_unstages_its_revoke", async () => {
    const m = await loaded();
    await m.user.click(screen.getByRole("button", { name: "Revoke grant option" }));
    expect(screen.getByRole("complementary", { name: "Pending changes" })).toBeInTheDocument();
    await m.user.click(screen.getByRole("button", { name: "Keep grant option" }));
    expect(screen.queryByRole("complementary", { name: "Pending changes" })).toBeNull();
    expect(screen.getByRole("button", { name: "Revoke grant option" })).toBeInTheDocument();
  });

  it("revoking_a_privilege_takes_its_grant_option_with_it", async () => {
    const m = await loaded();
    await m.user.click(screen.getByRole("button", { name: "Revoke grant option" }));
    await m.user.click(box("SELECT on invoices"));
    const panel = screen.getByRole("complementary", { name: "Pending changes" });
    expect(within(panel).getAllByRole("listitem").map((li) => li.textContent?.replace(/^\d+/, ""))).toEqual(["REVOKE SELECT ON billing.invoices"]);
  });

  it("an_owner_or_reserved_role_isnt_edited_here_and_isnt_read", async () => {
    const m = mountPage({ role: "chelabase_owner" });
    expect(await screen.findByText("Role chelabase_owner is managed by Chelabase, not here.")).toBeInTheDocument();
    expect(calls(m.f, "GET", PRIV)).toHaveLength(0);
  });

  it("says_when_the_privileges_cant_be_read", async () => {
    mountPage({ routes: { [`GET ${PRIV}`]: apiError(404, "not_found", "no Postgres database billing on this branch") } });
    expect(await screen.findByRole("alert")).toHaveTextContent("no Postgres database billing on this branch");
  });

  it("discard_clears_every_staged_edit_without_a_request", async () => {
    const m = await loaded();
    await m.user.click(box("DELETE on customers"));
    await m.user.click(screen.getByRole("switch", { name: "All rows on refunds" }));
    await m.user.click(screen.getByRole("button", { name: "Discard" }));
    expect(box("DELETE on customers")).not.toBeChecked();
    expect(screen.getByRole("switch", { name: "All rows on refunds" })).not.toBeChecked();
    expect(calls(m.f, "POST", CHANGES)).toHaveLength(0);
  });
});
