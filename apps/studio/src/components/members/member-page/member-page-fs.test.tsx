import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { expectThemedSelects } from "@/test/themed";
import { person } from "../hub/fixtures";
import {
  CONNECTION, D2, LOGIN_LATER, LOGIN_SOON, MARA, P1, P2, STRANGER, SOFIA, mount, ok, secret, sofiaFor, type Opts,
} from "./test-setup";

/** Fix S: the member page (finish-review fixes 2 and 5, BP-4, D10 and P36). */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const billing = () => screen.findByRole("region", { name: "Billing app" });
const RESET_D2 = `POST /v1/databases/${D2}/personal-login/reset-password`;
const self = (opts: Opts = {}) => mount({ caller: "self", memberId: SOFIA, members: () => [sofiaFor("self")], logins: () => [LOGIN_SOON, LOGIN_LATER], ...opts });
const mara = person(MARA, { name: "Mara Okafor", level: "tenant_admin", projects: [[P1, "admin"], [P2, "admin"]] });

describe("the grant card's footer: Save, Discard and Effective (fix 2, BP-4)", () => {
  it("a_clean_card_has_no_save_and_no_discard", async () => {
    mount({ caller: "tenant_admin" });
    const card = await billing();
    await within(card).findByRole("checkbox", { name: "billing_reader" });
    expect(within(card).queryByRole("button", { name: "Save" })).toBeNull();
    expect(within(card).queryByRole("button", { name: "Discard" })).toBeNull();
  });

  it("effective_reflects_unsaved_changes_and_discard_puts_it_back", async () => {
    const { user } = mount({ caller: "tenant_admin" });
    const card = await billing();
    const effective = () => within(card).getByText(/^Effective/).closest("p")!;
    await within(card).findByRole("checkbox", { name: "billing_reader" });
    expect(effective()).toHaveTextContent("Developer · billing_editor");
    expect(effective()).not.toHaveTextContent("billing_reader");

    await user.click(within(card).getByRole("checkbox", { name: "billing_reader" }));
    expect(effective()).toHaveTextContent("Developer · billing_editor, billing_reader");
    await user.click(within(card).getByRole("radio", { name: "Viewer" }));
    expect(effective()).toHaveTextContent("Viewer · billing_editor, billing_reader");

    await user.click(within(card).getByRole("button", { name: "Discard" }));
    expect(effective()).toHaveTextContent("Developer · billing_editor");
    expect(effective()).not.toHaveTextContent("billing_reader");
    expect(within(card).queryByRole("button", { name: "Save" })).toBeNull();
  });

  it("save_and_discard_sit_in_the_same_footer_as_effective_and_effective_names_roles_in_mono", async () => {
    const { user } = mount({ caller: "tenant_admin" });
    const card = await billing();
    await user.click(await within(card).findByRole("checkbox", { name: "billing_reader" }));
    const effective = within(card).getByText(/^Effective/).closest("p")!;
    const footer = effective.parentElement!;
    expect(footer).toContainElement(within(card).getByRole("button", { name: "Save" }));
    expect(footer).toContainElement(within(card).getByRole("button", { name: "Discard" }));
    for (const name of ["billing_editor", "billing_reader"]) {
      expect(within(effective).getByText(name).className).toContain("font-mono");
    }
  });

  it("a_card_added_for_a_project_without_a_grant_offers_save_in_its_footer", async () => {
    const { user } = mount({ caller: "tenant_admin" });
    await billing();
    await user.selectOptions(await screen.findByLabelText("Project to add"), "Ops console");
    await user.click(screen.getByRole("button", { name: "Add" }));
    const ops = await screen.findByRole("region", { name: "Ops console" });
    expect(await within(ops).findByRole("button", { name: "Save" })).toBeEnabled();
  });
});

describe("the header's initials (fix 5)", () => {
  it("uses_the_shared_first_and_last_word_rule", async () => {
    mount({ caller: "tenant_admin", members: () => [sofiaFor("tenant_admin", { display_name: "Ana Maria Lopez" }), mara] });
    await screen.findByRole("heading", { level: 1, name: /Ana Maria Lopez/ });
    expect(screen.getByText("AL")).toBeInTheDocument();
  });
});

describe("D10: the suspend banner", () => {
  const suspended = (extra: Parameters<typeof sofiaFor>[1]) => () => [sofiaFor("tenant_admin", { status: "suspended", ...extra }), mara];

  it("says_when_and_by_whom_from_the_roster", async () => {
    mount({ caller: "tenant_admin", members: suspended({ suspended_at: "2026-10-14T09:00:00Z", suspended_by: MARA }) });
    const banner = await screen.findByText(/Suspended on 14 Oct 2026 by Mara Okafor/);
    expect(banner).toBeInTheDocument();
    // The banner carries the rest of the explanation, as before.
    expect(banner.closest("div")).toHaveTextContent(/Sofia can't sign in to Studio/);
  });

  it("says_an_admin_when_nobody_is_recorded_and_never_shows_an_id", async () => {
    mount({ caller: "tenant_admin", members: suspended({ suspended_at: "2026-10-14T09:00:00Z", suspended_by: null }) });
    expect(await screen.findByText(/Suspended on 14 Oct 2026 by an admin/)).toBeInTheDocument();
  });

  it("a_suspender_who_is_no_longer_listed_is_not_shown_as_an_id", async () => {
    mount({ caller: "tenant_admin", members: suspended({ suspended_at: "2026-10-14T09:00:00Z", suspended_by: STRANGER }) });
    await screen.findByText(/Suspended on 14 Oct 2026 by /);
    expect(document.body).not.toHaveTextContent(STRANGER);
  });

  it("the_header_has_no_second_status_line", async () => {
    mount({ caller: "tenant_admin", members: suspended({ suspended_at: "2026-10-14T09:00:00Z", suspended_by: MARA }) });
    await screen.findByText(/Suspended on 14 Oct 2026/);
    // Fix S2: the header keeps its one status word; the banner adds the date and who, not a second status word.
    expect(screen.getAllByText("Suspended")).toHaveLength(1);
  });
});

describe("P36: a login row's menu", () => {
  const rowOf = async () => within(await screen.findByRole("table", { name: "Personal Postgres logins" })).getByRole("row", { name: /sofia_m/ });
  async function openMenu(user: ReturnType<typeof mount>["user"], row: HTMLElement) {
    if (!screen.queryByRole("menu")) await user.click(within(row).getByRole("button", { name: "More actions for sofia_m on warehouse" }));
    return screen.findByRole("menu");
  }

  it("the_row_itself_holds_no_default_role_select_and_no_copy_button", async () => {
    self();
    const row = await rowOf();
    expect(within(row).queryByRole("combobox")).toBeNull();
    expect(within(row).queryByRole("button", { name: /Copy connection string/ })).toBeNull();
    expect(row.querySelector("select")).toBeNull();
    // Delete stays on the row, neutral rather than coral.
    expect(within(row).getByRole("button", { name: "Delete" }).className).not.toContain("text-coral");
  });

  it("copy_connection_string_is_a_menu_item_and_a_chosen_default_role_goes_into_it", async () => {
    const m = self();
    const row = await rowOf();
    let menu = await openMenu(m.user, row);
    await m.user.click(within(menu).getByRole("menuitem", { name: "Copy connection string" }));
    expect(await navigator.clipboard.readText()).toBe("postgresql://sofia_m@h:5432/warehouse?sslmode=verify-full");

    menu = await openMenu(m.user, row);
    expect(within(menu).getByText("None")).toBeInTheDocument();
    await m.user.click(within(menu).getByText("etl_writer"));
    menu = await openMenu(m.user, row);
    await m.user.click(within(menu).getByRole("menuitem", { name: "Copy connection string" }));
    expect(await navigator.clipboard.readText()).toBe("postgresql://sofia_m@h:5432/warehouse?sslmode=verify-full&options=-c%20role%3Detl_writer");
    expect(await navigator.clipboard.readText()).not.toContain("q7Hc2vR9");
  });

  it("an_admin_looking_at_someone_elses_logins_gets_no_connection_items", async () => {
    const m = mount({ caller: "tenant_admin", logins: () => [LOGIN_LATER] });
    const row = await rowOf();
    const trigger = within(row).queryByRole("button", { name: /^More actions for/ });
    if (trigger) {
      await m.user.click(trigger);
      expect(screen.queryByRole("menuitem", { name: "Copy connection string" })).toBeNull();
    }
  });
});

describe("the reveal dialog (fix 5)", () => {
  async function reveal() {
    const m = self({ routes: { [RESET_D2]: ok(secret(LOGIN_LATER, { connection_string: CONNECTION })) } });
    const row = await rowOf();
    await m.user.click(within(row).getByRole("button", { name: "Reset password" }));
    const dialog = await screen.findByRole("dialog", { name: "Your login sofia_m on warehouse" });
    return { ...m, dialog };
  }
  const rowOf = async () => within(await screen.findByRole("table", { name: "Personal Postgres logins" })).getByRole("row", { name: /sofia_m/ });

  it("shows_the_whole_connection_string_in_a_field_that_wraps_not_a_single_line_input", async () => {
    const { dialog } = await reveal();
    const field = within(dialog).getByLabelText("Connection string · main branch");
    expect(field.tagName).not.toBe("INPUT");
    expect(field).toHaveValue(CONNECTION);
  });

  it("copy_buttons_are_labelled_and_sit_with_their_own_field", async () => {
    const { dialog } = await reveal();
    for (const [label, field] of [["Copy password", "Password"], ["Copy connection string", "Connection string · main branch"]] as const) {
      const button = within(dialog).getByRole("button", { name: label });
      const input = within(dialog).getByLabelText(field);
      expect(input.parentElement).toContainElement(button);
    }
  });

  it("the_default_role_select_is_themed", async () => {
    const { dialog } = await reveal();
    await waitFor(() => expect(dialog.querySelector("select")).not.toBeNull());
    expectThemedSelects(dialog);
  });
});

describe("themed selects on the page (fix 7)", () => {
  it("add_access_select_is_themed", async () => {
    const { container } = mount({ caller: "tenant_admin" });
    await billing();
    await screen.findByLabelText("Project to add");
    expectThemedSelects(container);
  });
});
