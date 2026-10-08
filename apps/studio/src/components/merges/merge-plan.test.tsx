import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { jsonResponse, sequence } from "@/test/fake-fetch";
import { detailOf, mergeOf, planStep } from "@/test/merge-fixtures";
import { MERGE_URL, bodies, renderMergePage } from "@/test/merge-page-harness";

const computing = () => jsonResponse(mergeOf({ status: "computing", version: 4 }));

describe("MergePlan", () => {
  it("groups the steps by phase", async () => {
    renderMergePage(
      mergeOf({
        plan: {
          steps: [
            planStep("role:reporter", { phase: "roles", sql: "-- create role reporter" }),
            planStep("g1", { sql: "ALTER TABLE t ADD c int" }),
            planStep("ix", { phase: "online", sql: "CREATE INDEX CONCURRENTLY ix ON t (c)" }),
          ],
        },
      }),
    );
    const plan = await screen.findByRole("region", { name: "Plan" });
    const phases = within(plan)
      .getAllByRole("heading", { level: 3 })
      .map((h) => h.textContent)
      .filter((t) => t?.startsWith("Phase"));
    expect(phases).toEqual(["Phase 0: roles", "Phase 1: one transaction", "Phase 2: online, one statement at a time"]);
    expect(plan).toHaveTextContent("CREATE INDEX CONCURRENTLY ix ON t (c)");
  });

  it("acknowledgements_required_before_apply_button_enables", async () => {
    const drop = planStep("drop:t.email", { sql: "ALTER TABLE t DROP COLUMN email", data_loss: true, items: ["column:1:2"] });
    const bypass = planStep("role:ops", { phase: "roles", sql: "-- alter role ops bypassrls", security: true });
    const rehearsed = mergeOf({ status: "rehearsed", plan: { steps: [drop, bypass] } });
    const { fetch, unmount } = renderMergePage(rehearsed, { routes: { [`PATCH ${MERGE_URL}`]: computing } });
    const apply = await screen.findByRole("button", { name: "Apply to main" });
    expect(apply).toBeDisabled();
    expect(apply).toHaveAccessibleDescription("Acknowledge 2 steps that lose data, change security or break clients.");
    // Risky states in plain words.
    expect(screen.getByText("Loses data: the dropped or rewritten values can't be recovered.")).toBeInTheDocument();
    expect(screen.getByText(/Changes security: a role that bypasses row-level security/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("checkbox", { name: "I accept the data loss of drop:t.email" }));
    await userEvent.click(screen.getByRole("checkbox", { name: "I accept the security change of role:ops" }));
    await userEvent.click(screen.getByRole("button", { name: "Save acknowledgements" }));
    await vi.waitFor(async () =>
      expect(await bodies(fetch, "PATCH", MERGE_URL)).toEqual([{ version: 3, acknowledgements: ["column:1:2", "role:ops"] }]),
    );
    unmount();

    renderMergePage({ ...rehearsed, acknowledgements: ["column:1:2", "role:ops"] });
    expect(await screen.findByRole("button", { name: "Apply to main" })).toBeEnabled();
    expect(screen.getByRole("checkbox", { name: "I accept the data loss of drop:t.email" })).toBeChecked();
  });

  it("keeps a rehearsed merge rehearsed when only acknowledgements are saved, and says so (E34)", async () => {
    const drop = planStep("drop:t.email", { data_loss: true, items: ["column:1:2"] });
    const rehearsed = mergeOf({ status: "rehearsed", plan: { steps: [drop] } });
    const saved = mergeOf({ status: "rehearsed", version: 4, acknowledgements: ["column:1:2"], plan: { steps: [drop] } });
    const { fetch } = renderMergePage(rehearsed, {
      get: sequence(
        () => jsonResponse(detailOf(rehearsed)),
        () => jsonResponse(detailOf(saved)),
      ),
      routes: { [`PATCH ${MERGE_URL}`]: () => jsonResponse(saved) },
    });
    const plan = await screen.findByRole("region", { name: "Plan" });
    expect(plan).not.toHaveTextContent("returns a rehearsed merge to draft");
    expect(plan).toHaveTextContent("Saving acknowledgements keeps the rehearsal");
    await userEvent.click(within(plan).getByRole("checkbox", { name: "I accept the data loss of drop:t.email" }));
    await userEvent.click(within(plan).getByRole("button", { name: "Save acknowledgements" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "PATCH", MERGE_URL)).toHaveLength(1));
    // Still rehearsed after the save, so Apply is there once it is acknowledged.
    expect(await screen.findByRole("button", { name: "Apply to main" })).toBeEnabled();
  });

  it("flags a role rename as breaking, asks for its acknowledgement and lists the keys that follow it (E29a)", async () => {
    const rename = planStep("role:rename:a", {
      phase: "roles",
      sql: "ALTER ROLE a RENAME TO b",
      breaking: { role_rename: { role: "a", new_name: "b" } },
    });
    const rehearsed = mergeOf({ status: "rehearsed", plan: { steps: [rename] } });
    const key = { id: "k1", name: "ci", kind: "secret", role: "a", prefix: "sk_", created_at: "2026-10-01T00:00:00Z", last_used_at: null, bypasses_rls: false, role_blocked: false, databases: null };
    const other = { ...key, id: "k2", name: "other", role: "z" };
    const { fetch } = renderMergePage(rehearsed, {
      routes: {
        "GET /v1/databases/db/branches/b-main/keys": () => jsonResponse([key, other, { ...key, id: "k3", name: "public", kind: "public" }]),
        "GET /v1/databases/db/branches/b-main/roles/a/sessions": () =>
          jsonResponse({
            compute_running: true,
            count: 2,
            sessions: [
              { client_addr: "10.0.0.7", application_name: "api", backend_start: "2026-10-08T10:00:00Z" },
              { client_addr: null, application_name: "", backend_start: "2026-10-08T11:30:00Z" },
            ],
          }),
        [`PATCH ${MERGE_URL}`]: () => jsonResponse(mergeOf({ status: "rehearsed", version: 4, acknowledgements: ["role:rename:a"] })),
      },
    });
    const plan = await screen.findByRole("region", { name: "Plan" });
    expect(within(plan).getByText("Breaks clients")).toBeInTheDocument();
    expect(plan).toHaveTextContent("Renames role a to b: anything that connects to main as a will fail to authenticate after the apply.");
    expect(await within(plan).findByText(/Secret keys that use a: ci\. They move with the role and keep working\./)).toBeInTheDocument();
    expect(plan).not.toHaveTextContent("other");
    // The live sessions on the target (E29a), beside the keys.
    const sessions = await within(plan).findByRole("list", { name: "Sessions connected to main as a" });
    expect(within(plan).getByText("Connected to main as a right now: 2 sessions.")).toBeInTheDocument();
    const items = within(sessions).getAllByRole("listitem");
    expect(items[0]).toHaveTextContent("10.0.0.7 · api · since 2026-10-08 10:00 UTC");
    expect(items[1]).toHaveTextContent("inside the compute · no application name · since 2026-10-08 11:30 UTC");
    // Clients not connected now can't be listed; the text says what to do about them.
    expect(plan).toHaveTextContent("Clients that aren't connected right now aren't listed: change every connection string that names a to b.");
    expect(screen.getByRole("button", { name: "Apply to main" })).toBeDisabled();
    await userEvent.click(within(plan).getByRole("checkbox", { name: "I accept the breaking change of role:rename:a" }));
    await userEvent.click(within(plan).getByRole("button", { name: "Save acknowledgements" }));
    await vi.waitFor(async () =>
      expect(await bodies(fetch, "PATCH", MERGE_URL)).toEqual([{ version: 3, acknowledgements: ["role:rename:a"] }]),
    );
  });

  it("says a role rename has no keys when none use it", async () => {
    const rename = planStep("role:rename:a", { phase: "roles", breaking: { role_rename: { role: "a", new_name: "b" } } });
    renderMergePage(mergeOf({ plan: { steps: [rename] } }), {
      routes: { "GET /v1/databases/db/branches/b-main/keys": () => jsonResponse([]) },
    });
    expect(await screen.findByText("No secret key uses a.")).toBeInTheDocument();
  });

  it("says when no one is connected as a renamed role, or the target's compute isn't running (E29a)", async () => {
    const rename = planStep("role:rename:a", { phase: "roles", breaking: { role_rename: { role: "a", new_name: "b" } } });
    const SESSIONS = "GET /v1/databases/db/branches/b-main/roles/a/sessions";
    const { unmount } = renderMergePage(mergeOf({ plan: { steps: [rename] } }), {
      routes: { [SESSIONS]: () => jsonResponse({ compute_running: true, count: 0, sessions: [] }) },
    });
    expect(await screen.findByText("No one is connected to main as a right now.")).toBeInTheDocument();
    unmount();
    renderMergePage(mergeOf({ plan: { steps: [rename] } }), {
      routes: { [SESSIONS]: () => jsonResponse({ compute_running: false, count: 0, sessions: [] }) },
    });
    expect(await screen.findByText("main's compute isn't running, so no one is connected as a right now.")).toBeInTheDocument();
  });

  it("says when the live sessions couldn't be read", async () => {
    const rename = planStep("role:rename:a", { phase: "roles", breaking: { role_rename: { role: "a", new_name: "b" } } });
    renderMergePage(mergeOf({ plan: { steps: [rename] } }), {
      routes: {
        "GET /v1/databases/db/branches/b-main/roles/a/sessions": () =>
          jsonResponse({ error: { code: "worker_unavailable", message: "worker unavailable" } }, 503),
      },
    });
    expect(await screen.findByText(/The live sessions couldn't be read: worker unavailable/)).toBeInTheDocument();
  });

  it("says an owner-level role is a security change too (E28)", async () => {
    const owner = planStep("role:update:ops", { phase: "roles", sql: "-- ops joins neon_superuser", security: true });
    renderMergePage(mergeOf({ status: "rehearsed", plan: { steps: [owner] } }));
    const plan = await screen.findByRole("region", { name: "Plan" });
    expect(plan).toHaveTextContent("an owner-level role (superuser, CREATEROLE, CREATEDB, REPLICATION or a member of neon_superuser) reaches even more");
  });

  it("user_step_insert_and_move", async () => {
    const plan = { steps: [planStep("g1"), planStep("g2")] };
    const { fetch, unmount } = renderMergePage(mergeOf({ plan }), { routes: { [`PATCH ${MERGE_URL}`]: computing } });
    await userEvent.selectOptions(await screen.findByLabelText("Insert after"), "g1");
    await userEvent.type(screen.getByLabelText("Step SQL"), "UPDATE t SET c = 0");
    await userEvent.click(screen.getByRole("button", { name: "Add step" }));
    await vi.waitFor(async () =>
      expect(await bodies(fetch, "PATCH", MERGE_URL)).toEqual([
        { version: 3, steps: [{ id: "step-1", after: "g1", sql: "UPDATE t SET c = 0" }] },
      ]),
    );
    unmount();

    // The recomputed plan has it after g1: move it down, past g2.
    const withStep = mergeOf({
      steps: [{ id: "step-1", after: "g1", sql: "UPDATE t SET c = 0" }],
      plan: { steps: [planStep("g1"), planStep("step-1", { kind: "user", sql: "UPDATE t SET c = 0" }), planStep("g2")] },
    });
    const second = renderMergePage(withStep, { routes: { [`PATCH ${MERGE_URL}`]: computing } });
    expect(await screen.findByRole("button", { name: "Move step-1 up" })).toBeEnabled();
    await userEvent.click(screen.getByRole("button", { name: "Move step-1 down" }));
    await vi.waitFor(async () =>
      expect(await bodies(second.fetch, "PATCH", MERGE_URL)).toEqual([
        { version: 3, steps: [{ id: "step-1", after: "g2", sql: "UPDATE t SET c = 0" }] },
      ]),
    );
  });

  it("deletes an own step", async () => {
    const withStep = mergeOf({
      steps: [{ id: "step-1", after: "g1", sql: "select 1" }],
      plan: { steps: [planStep("g1"), planStep("step-1", { kind: "user", sql: "select 1" })] },
    });
    const { fetch } = renderMergePage(withStep, { routes: { [`PATCH ${MERGE_URL}`]: computing } });
    await userEvent.click(await screen.findByRole("button", { name: "Delete step-1" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "PATCH", MERGE_URL)).toEqual([{ version: 3, steps: [] }]));
  });

  it("shows a refused step's error next to it", async () => {
    renderMergePage(
      mergeOf({
        plan: null,
        steps: [{ id: "step-1", after: "ix", sql: "ALTER TABLE t ADD d int" }],
        error:
          "1 statement(s) can't run in a merge\nstep step-1: this statement runs in phase 1 but follows a phase 2 statement, so it would run before it; place it before the phase 2 statements",
      }),
    );
    const step = await screen.findByRole("listitem", { name: "Your step step-1" });
    expect(within(step).getByRole("alert")).toHaveTextContent(
      "this statement runs in phase 1 but follows a phase 2 statement, so it would run before it",
    );
  });

  it("saves the lock timeout", async () => {
    const { fetch } = renderMergePage(mergeOf({ plan: { steps: [planStep("g1")] }, options: { lock_timeout_ms: 5000 } }), {
      routes: { [`PATCH ${MERGE_URL}`]: computing },
    });
    const input = await screen.findByLabelText("Lock timeout (ms)");
    expect(input).toHaveValue(5000);
    await userEvent.clear(input);
    await userEvent.type(input, "2000");
    await userEvent.click(screen.getByRole("button", { name: "Save lock timeout" }));
    await vi.waitFor(async () =>
      expect(await bodies(fetch, "PATCH", MERGE_URL)).toEqual([{ version: 3, options: { lock_timeout_ms: 2000 } }]),
    );
  });
});
