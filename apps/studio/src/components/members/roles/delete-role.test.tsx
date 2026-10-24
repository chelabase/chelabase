import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B1, D1, PG, ROLE, ROLES_URL, apiError, calls, mountPage, query } from "./test-setup";

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const ROLE_URL = `${ROLES_URL}/${ROLE}`;
const PREVIEW = `${ROLE_URL}/delete-preview`;
const QUARANTINE = "chelabase_orphaned";

const exposure = (identity: string, extra: object = {}) => ({
  database: "billing",
  kind: "function",
  identity,
  grantees: ["billing_reader", "support_agent"],
  ...extra,
});
const E1 = exposure("billing.issue_refund(uuid, numeric)");
const E2 = exposure("billing.open_balances", { kind: "view", grantees: ["PUBLIC"] });

type Preview = { successor: string; exposures: unknown[]; confirm: string | null };
const previewOf = (fn: (successor: string) => Preview): FakeHandler => (req) => {
  const successor = new URL(req.url).searchParams.get("successor") ?? QUARANTINE;
  return jsonResponse(fn(successor));
};
const clean = (successor: string): Preview => ({ successor, exposures: [], confirm: null });
const DONE = { successor: QUARANTINE, reassigned: { count: 3, definers: [] as string[] } };

/** The role page's Danger zone, with the delete dialog opened. */
async function openDialog(routes: Record<string, FakeHandler> = {}, opts: Parameters<typeof mountPage>[0] = {}) {
  const m = mountPage({
    routes: { [`GET ${PREVIEW}`]: previewOf(clean), [`DELETE ${ROLE_URL}`]: () => jsonResponse(DONE), ...routes },
    ...opts,
  });
  await screen.findByRole("heading", { level: 1, name: ROLE });
  await m.user.click(screen.getByRole("tab", { name: "Danger zone" }));
  await m.user.click(await screen.findByRole("button", { name: /^Delete role/ }));
  const dialog = await screen.findByRole("dialog", { name: `Delete role ${ROLE}` });
  return { ...m, dialog };
}
const deleteButton = (dialog: HTMLElement) => within(dialog).getByRole("button", { name: /^Delete role$|^Delete$/ });
const successorSelect = (dialog: HTMLElement) => within(dialog).getByLabelText("Successor") as HTMLSelectElement;

describe("the Danger zone", () => {
  it("is_for_admins_only", async () => {
    mountPage({ caller: "developer" });
    await screen.findByRole("heading", { level: 1, name: ROLE });
    expect(screen.queryByRole("tab", { name: "Danger zone" })).toBeNull();
  });

  it("explains_what_happens_to_the_roles_objects_before_asking", async () => {
    const m = mountPage();
    await screen.findByRole("heading", { level: 1, name: ROLE });
    await m.user.click(screen.getByRole("tab", { name: "Danger zone" }));
    const panel = await screen.findByRole("tabpanel");
    expect(within(panel).getByText(/objects.*go to the successor/i)).toBeInTheDocument();
    expect(calls(m.f, "GET", PREVIEW)).toHaveLength(0);
  });
});

describe("successor picker", () => {
  it("defaults_to_quarantine_and_offers_roles_the_owner_can_hand_objects_to", async () => {
    const { dialog } = await openDialog();
    const select = successorSelect(dialog);
    const options = within(select).getAllByRole("option") as HTMLOptionElement[];
    expect(options[0]).toHaveValue(QUARANTINE);
    expect(options[0]).toHaveTextContent("Quarantine (no one)");
    expect(select.value).toBe(QUARANTINE);
    const values = options.map((o) => o.value);
    for (const v of ["billing_reader", "support_agent", "webhook_worker"]) expect(values).toContain(v);
    // Never itself, the owner, an owner-level role, a reserved name or a personal login.
    for (const v of [ROLE, "chelabase_owner", "billing_admin", "authenticator", "chelabase_schema_locked", "sofia"]) expect(values).not.toContain(v);
  });

  it("previews_the_default_successor_when_it_opens", async () => {
    const m = await openDialog();
    await waitFor(() => expect(calls(m.f, "GET", PREVIEW)).toHaveLength(1));
    expect(query(calls(m.f, "GET", PREVIEW)[0]).get("successor")).toBe(QUARANTINE);
  });

  it("previews_again_for_the_chosen_successor", async () => {
    const m = await openDialog();
    await m.user.selectOptions(successorSelect(m.dialog), "billing_reader");
    await waitFor(() => expect(calls(m.f, "GET", PREVIEW)).toHaveLength(2));
    expect(query(calls(m.f, "GET", PREVIEW)[1]).get("successor")).toBe("billing_reader");
  });
});

describe("preview and confirm (T10)", () => {
  it("deletes_to_quarantine_without_a_confirm_token", async () => {
    const m = await openDialog();
    await waitFor(() => expect(deleteButton(m.dialog)).toBeEnabled());
    expect(within(m.dialog).queryByRole("checkbox")).toBeNull();
    await m.user.click(deleteButton(m.dialog));
    await waitFor(() => expect(calls(m.f, "DELETE", ROLE_URL)).toHaveLength(1));
    const q = query(calls(m.f, "DELETE", ROLE_URL)[0]);
    expect(q.get("successor")).toBe(QUARANTINE);
    expect(q.has("confirm")).toBe(false);
  });

  it("lists_what_a_named_successor_would_expose_and_requires_confirming_it", async () => {
    const m = await openDialog({ [`GET ${PREVIEW}`]: previewOf((s) => (s === QUARANTINE ? clean(s) : { successor: s, exposures: [E1, E2], confirm: "tok-1" })) });
    await m.user.selectOptions(successorSelect(m.dialog), "billing_reader");
    // The exposures: database, kind, identity and who gets to use it.
    const first = await within(m.dialog).findByText("billing.issue_refund(uuid, numeric)");
    const row = first.closest("li, tr")!;
    expect(row).toHaveTextContent("billing");
    expect(row).toHaveTextContent(/function/i);
    expect(row).toHaveTextContent("billing_reader");
    expect(row).toHaveTextContent("support_agent");
    const view = within(m.dialog).getByText("billing.open_balances").closest("li, tr")!;
    expect(view).toHaveTextContent(/view/i);
    expect(view).toHaveTextContent("PUBLIC");

    // Delete waits for the confirmation.
    const confirm = within(m.dialog).getByRole("checkbox", { name: /I understand/ });
    expect(confirm).not.toBeChecked();
    expect(deleteButton(m.dialog)).toBeDisabled();
    await m.user.click(confirm);
    await m.user.click(deleteButton(m.dialog));
    await waitFor(() => expect(calls(m.f, "DELETE", ROLE_URL)).toHaveLength(1));
    const q = query(calls(m.f, "DELETE", ROLE_URL)[0]);
    expect(q.get("successor")).toBe("billing_reader");
    expect(q.get("confirm")).toBe("tok-1");
  });

  it("lists_what_quarantine_would_find_but_asks_for_no_confirmation", async () => {
    const m = await openDialog({ [`GET ${PREVIEW}`]: previewOf((s) => ({ successor: s, exposures: [E1], confirm: null })) });
    expect(await within(m.dialog).findByText("billing.issue_refund(uuid, numeric)")).toBeInTheDocument();
    expect(within(m.dialog).queryByRole("checkbox")).toBeNull();
    expect(deleteButton(m.dialog)).toBeEnabled();
    await m.user.click(deleteButton(m.dialog));
    await waitFor(() => expect(calls(m.f, "DELETE", ROLE_URL)).toHaveLength(1));
    expect(query(calls(m.f, "DELETE", ROLE_URL)[0]).has("confirm")).toBe(false);
  });

  it("changing_the_successor_clears_the_confirmation", async () => {
    const m = await openDialog({ [`GET ${PREVIEW}`]: previewOf((s) => (s === QUARANTINE ? clean(s) : { successor: s, exposures: [E1], confirm: `tok-${s}` })) });
    const select = successorSelect(m.dialog);
    await m.user.selectOptions(select, "billing_reader");
    await m.user.click(await within(m.dialog).findByRole("checkbox", { name: /I understand/ }));
    expect(deleteButton(m.dialog)).toBeEnabled();
    await m.user.selectOptions(select, "support_agent");
    const again = await within(m.dialog).findByRole("checkbox", { name: /I understand/ });
    await waitFor(() => expect(again).not.toBeChecked());
    expect(deleteButton(m.dialog)).toBeDisabled();
    await m.user.click(again);
    await m.user.click(deleteButton(m.dialog));
    await waitFor(() => expect(calls(m.f, "DELETE", ROLE_URL)).toHaveLength(1));
    // The token of the successor chosen last, not the first one's.
    expect(query(calls(m.f, "DELETE", ROLE_URL)[0]).get("confirm")).toBe("tok-support_agent");
  });

  it("waits_for_the_preview_before_it_lets_you_delete", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    const m = await openDialog({
      [`GET ${PREVIEW}`]: async () => {
        await gate;
        return jsonResponse(clean(QUARANTINE));
      },
    });
    expect(deleteButton(m.dialog)).toBeDisabled();
    release();
    await waitFor(() => expect(deleteButton(m.dialog)).toBeEnabled());
  });

  it("a_preview_that_fails_blocks_the_delete_and_says_why", async () => {
    const m = await openDialog({ [`GET ${PREVIEW}`]: apiError(422, "invalid_successor", "that role can't take the objects") });
    expect(await within(m.dialog).findByRole("alert")).toHaveTextContent("that role can't take the objects");
    expect(deleteButton(m.dialog)).toBeDisabled();
    expect(calls(m.f, "DELETE", ROLE_URL)).toHaveLength(0);
  });

  it("exposure_unconfirmed_shows_the_fresh_list_and_needs_a_new_confirmation", async () => {
    let attempts = 0;
    const m = await openDialog({
      [`GET ${PREVIEW}`]: previewOf((s) => (s === QUARANTINE ? clean(s) : { successor: s, exposures: [E1], confirm: "tok-1" })),
      [`DELETE ${ROLE_URL}`]: () =>
        ++attempts === 1
          ? jsonResponse(
              { error: { code: "exposure_unconfirmed", message: "review `exposures` and delete again with `confirm`", exposures: [E1, E2], confirm: "tok-2" } },
              409,
            )
          : jsonResponse({ successor: "billing_reader", reassigned: { count: 4, definers: [] } }),
    });
    await m.user.selectOptions(successorSelect(m.dialog), "billing_reader");
    await m.user.click(await within(m.dialog).findByRole("checkbox", { name: /I understand/ }));
    await m.user.click(deleteButton(m.dialog));

    // Someone granted something meanwhile: the fresh list shows, and the old confirmation no longer counts.
    expect(await within(m.dialog).findByText("billing.open_balances")).toBeInTheDocument();
    expect(within(m.dialog).getByText("billing.issue_refund(uuid, numeric)")).toBeInTheDocument();
    const again = within(m.dialog).getByRole("checkbox", { name: /I understand/ });
    expect(again).not.toBeChecked();
    expect(deleteButton(m.dialog)).toBeDisabled();
    expect(screen.queryByText(/Deleted/)).toBeNull();

    await m.user.click(again);
    await m.user.click(deleteButton(m.dialog));
    await waitFor(() => expect(calls(m.f, "DELETE", ROLE_URL)).toHaveLength(2));
    expect(query(calls(m.f, "DELETE", ROLE_URL)[0]).get("confirm")).toBe("tok-1");
    expect(query(calls(m.f, "DELETE", ROLE_URL)[1]).get("confirm")).toBe("tok-2");
  });
});

describe("after the delete", () => {
  it("lists_the_security_definer_routines_that_moved_for_review_then_returns_to_the_roles_tab", async () => {
    const definers = ["billing.issue_refund(uuid, numeric)", "billing.recalc_totals(uuid)"];
    const m = await openDialog({ [`DELETE ${ROLE_URL}`]: () => jsonResponse({ successor: QUARANTINE, reassigned: { count: 7, definers } }) });
    await waitFor(() => expect(deleteButton(m.dialog)).toBeEnabled());
    await m.user.click(deleteButton(m.dialog));
    const dialog = await screen.findByRole("dialog", { name: /billing_editor/ });
    expect(await within(dialog).findByText(/Deleted billing_editor/)).toBeInTheDocument();
    expect(within(dialog).getByText(/7 objects/)).toBeInTheDocument();
    expect(within(dialog).getByText(QUARANTINE, { exact: false })).toBeInTheDocument();
    expect(within(dialog).getByText(/SECURITY DEFINER/)).toBeInTheDocument();
    for (const d of definers) expect(within(dialog).getByText(d)).toBeInTheDocument();

    await m.user.click(within(dialog).getByRole("button", { name: "Done" }));
    await waitFor(() => expect(m.router.state.location.pathname).toBe("/app/members"));
    expect(m.router.state.location.search).toMatchObject({ tab: "roles", database: D1, branch: B1, pg_database: PG });
  });

  it("says_nothing_about_definers_when_none_moved", async () => {
    const m = await openDialog();
    await waitFor(() => expect(deleteButton(m.dialog)).toBeEnabled());
    await m.user.click(deleteButton(m.dialog));
    const dialog = await screen.findByRole("dialog", { name: /billing_editor/ });
    await within(dialog).findByText(/Deleted billing_editor/);
    expect(within(dialog).queryByText(/SECURITY DEFINER/)).toBeNull();
  });
});

describe("refusals", () => {
  it("a_role_in_use_by_data_api_keys_names_the_keys_and_stays_open", async () => {
    const m = await openDialog({
      [`DELETE ${ROLE_URL}`]: apiError(409, "role_in_use", "a data API key uses this role", { keys: [{ id: "k1", name: "web-backend" }, { id: "k2", name: "cron" }] }),
    });
    await waitFor(() => expect(deleteButton(m.dialog)).toBeEnabled());
    await m.user.click(deleteButton(m.dialog));
    const alert = await within(m.dialog).findByRole("alert");
    expect(alert).toHaveTextContent("web-backend");
    expect(alert).toHaveTextContent("cron");
    expect(screen.getByRole("dialog", { name: `Delete role ${ROLE}` })).toBeInTheDocument();
    expect(m.router.state.location.pathname).toContain("/app/members/roles/");
  });

  it("an_invalid_successor_is_shown_and_the_dialog_stays", async () => {
    const m = await openDialog({ [`DELETE ${ROLE_URL}`]: apiError(422, "invalid_successor", "the successor can't take the role's objects") });
    await waitFor(() => expect(deleteButton(m.dialog)).toBeEnabled());
    await m.user.click(deleteButton(m.dialog));
    expect(await within(m.dialog).findByRole("alert")).toHaveTextContent("the successor can't take the role's objects");
    expect(deleteButton(m.dialog)).toBeEnabled();
  });

  it("cancel_closes_the_dialog_without_deleting", async () => {
    const m = await openDialog();
    await m.user.click(within(m.dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls(m.f, "DELETE", ROLE_URL)).toHaveLength(0);
  });
});
