import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { jsonResponse, sequence } from "@/test/fake-fetch";
import { detailOf } from "@/test/merge-fixtures";
import { devBranch, mainBranch, mergeOf, planStep } from "@/test/merge-fixtures";
import { bodies, renderMergePage } from "@/test/merge-page-harness";

const APPLY = "/v1/databases/db/merges/m1/apply";
const plan = { steps: [planStep("g1"), planStep("ix", { phase: "online" })] };
const applying = () => jsonResponse(mergeOf({ status: "applying" }), 202);

describe("MergeApply", () => {
  it("apply_on_protected_target_asks_confirm", async () => {
    const { fetch } = renderMergePage(mergeOf({ status: "rehearsed", plan }), {
      branches: [{ ...mainBranch, protected: true }, devBranch],
      routes: { [`POST ${APPLY}`]: applying },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Apply to main" }));
    const confirm = screen.getByRole("region", { name: "Confirm the apply" });
    expect(confirm).toHaveTextContent("main is protected. Applying changes its schema for everything that uses it");
    expect(await bodies(fetch, "POST", APPLY)).toEqual([]);
    await userEvent.click(within(confirm).getByRole("button", { name: "Apply to protected main" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "POST", APPLY)).toEqual([{ confirm: true, version: 3 }]));
  });

  it("asks for the confirm when the API says the target is protected, and reloads the branches", async () => {
    const { fetch } = renderMergePage(mergeOf({ status: "rehearsed", plan }), {
      // The list was read before main was protected.
      routes: {
        [`POST ${APPLY}`]: (req) =>
          req.clone().json().then((b: { confirm?: boolean }) =>
            b.confirm
              ? applying()
              : jsonResponse({ error: { code: "merge_confirm_required", message: "raw" } }, 409),
          ),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Apply to main" }));
    const confirm = await screen.findByRole("region", { name: "Confirm the apply" });
    expect(confirm).toHaveTextContent("main is protected.");
    await vi.waitFor(() =>
      expect(fetch.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname === "/v1/databases/db/branches")).toHaveLength(2),
    );
    await userEvent.click(within(confirm).getByRole("button", { name: "Apply to protected main" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "POST", APPLY)).toEqual([{ version: 3 }, { confirm: true, version: 3 }]));
  });

  it("sends the version it saw, so a save since (a new after-merge choice) is applied as seen", async () => {
    const { fetch } = renderMergePage(mergeOf({ status: "rehearsed", plan, version: 7 }), { routes: { [`POST ${APPLY}`]: applying } });
    await userEvent.click(await screen.findByRole("button", { name: "Apply to main" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "POST", APPLY)).toEqual([{ version: 7 }]));
  });

  it("reloads the merge when it changed since the page read it, and says so", async () => {
    const theirs = mergeOf({ status: "rehearsed", plan, version: 9, acknowledgements: ["x"] });
    const { fetch } = renderMergePage(mergeOf({ status: "rehearsed", plan }), {
      get: sequence(
        () => jsonResponse(detailOf(mergeOf({ status: "rehearsed", plan }))),
        () => jsonResponse(detailOf(theirs)),
      ),
      routes: {
        [`POST ${APPLY}`]: sequence(
          () => jsonResponse({ error: { code: "merge_version_conflict", message: "raw", current: theirs } }, 409),
          applying,
        ),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Apply to main" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("This merge changed elsewhere and was reloaded. Check it and save again.");
    // What the page holds is theirs now, so the next apply is of that version.
    await vi.waitFor(async () => expect(screen.getByRole("button", { name: "Apply to main" })).toBeEnabled());
    await userEvent.click(screen.getByRole("button", { name: "Apply to main" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "POST", APPLY)).toEqual([{ version: 3 }, { version: 9 }]));
  });

  it("shows one Apply button while the protected confirm is open", async () => {
    renderMergePage(mergeOf({ status: "rehearsed", plan }), { branches: [{ ...mainBranch, protected: true }, devBranch] });
    await userEvent.click(await screen.findByRole("button", { name: "Apply to main" }));
    const confirm = screen.getByRole("region", { name: "Confirm the apply" });
    expect(screen.queryByRole("button", { name: "Apply to main" })).toBeNull();
    expect(within(confirm).getByRole("button", { name: "Apply to protected main" })).toBeInTheDocument();
    await userEvent.click(within(confirm).getByRole("button", { name: "Cancel" }));
    expect(await screen.findByRole("button", { name: "Apply to main" })).toBeEnabled();
  });

  it("keeps Apply off until the branches are loaded", async () => {
    renderMergePage(mergeOf({ status: "rehearsed", plan }), {
      routes: { "GET /v1/databases/db/branches": () => new Promise<never>(() => {}) },
    });
    const apply = await screen.findByRole("button", { name: /^Apply to/ });
    expect(apply).toBeDisabled();
    expect(apply).toHaveAccessibleDescription("Loading the branches…");
  });

  it("applies to an unprotected target without a confirm", async () => {
    const { fetch } = renderMergePage(mergeOf({ status: "rehearsed", plan }), { routes: { [`POST ${APPLY}`]: applying } });
    await userEvent.click(await screen.findByRole("button", { name: "Apply to main" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "POST", APPLY)).toEqual([{ version: 3 }]));
  });

  it("links the merge that is busy on the target", async () => {
    renderMergePage(mergeOf({ status: "rehearsed", plan }), {
      routes: {
        [`POST ${APPLY}`]: () => jsonResponse({ error: { code: "merge_busy", message: "raw", merge_id: "m2" } }, 409),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Apply to main" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Another merge is changing the target branch.");
    expect(screen.getByRole("link", { name: "Open the merge" })).toHaveAttribute("href", "/app/projects/p1/databases/db/merges/m2");
  });

  it("shows progress by phase from the log", async () => {
    renderMergePage(mergeOf({ status: "online", plan }), {
      events: [
        { at: "2026-10-06T10:00:00Z", kind: "status", step_id: null, phase: null, outcome: null, duration_ms: null, detail: { status: "applying" } },
        { at: "2026-10-06T10:00:01Z", kind: "step", step_id: "g1", phase: "tx", outcome: "ok", duration_ms: 40, detail: null },
      ],
    });
    const apply = await screen.findByRole("region", { name: "Apply" });
    expect(within(apply).getByText("Phase 1: one transaction: 1 of 1 steps done")).toBeInTheDocument();
    expect(within(apply).getByText("Phase 2: online, one statement at a time: 0 of 1 steps done")).toBeInTheDocument();
    const log = within(apply).getByRole("list", { name: "Merge log" });
    expect(log).toHaveTextContent("step g1: ok (40 ms)");
  });

  it("counts the plan's transaction as done once phase1 is logged", async () => {
    renderMergePage(mergeOf({ status: "online", plan: { steps: [planStep("g1"), planStep("g2"), planStep("ix", { phase: "online" })] } }), {
      events: [
        { at: "2026-10-06T10:00:00Z", kind: "status", step_id: null, phase: null, outcome: null, duration_ms: null, detail: { to: "applying" } },
        { at: "2026-10-06T10:00:01Z", kind: "step", step_id: "phase1", phase: "tx", outcome: "ok", duration_ms: 400, detail: null },
      ],
    });
    const apply = await screen.findByRole("region", { name: "Apply" });
    expect(within(apply).getByText("Phase 1: one transaction: 2 of 2 steps done")).toBeInTheDocument();
    expect(within(apply).getByText("Phase 2: online, one statement at a time: 0 of 1 steps done")).toBeInTheDocument();
  });

  it.each([
    ["roles", /Some roles may already be on main/],
    ["enum_add", /Some enum values are already on main/],
    ["tx", /The plan's transaction should have rolled back; Retry checks first whether it committed after all\./],
  ])("retries a merge that failed in %s from the start, and says what stays", async (phase, text) => {
    const { fetch } = renderMergePage(mergeOf({ status: "failed", failed_phase: phase, error: "permission denied", plan }), {
      routes: { "POST /v1/databases/db/merges/m1/retry": () => jsonResponse(mergeOf({ status: "applying" }), 202) },
    });
    const apply = await screen.findByRole("region", { name: "Apply" });
    const failure = within(apply).getByRole("alert");
    expect(failure).toHaveTextContent(text);
    expect(failure).toHaveTextContent("Roles and enum values the first attempt made stay on main");
    expect(failure).toHaveTextContent("permission denied");
    // An unread transaction may have committed (F3 Q3): never stated as rolled back for sure.
    expect(failure).not.toHaveTextContent("none of its changes are in");
    // The online block is for the online phase only.
    expect(within(apply).queryByRole("button", { name: "Retry the online steps" })).toBeNull();
    expect(within(apply).queryByRole("button", { name: "Abandon the merge" })).toBeNull();
    await userEvent.click(within(apply).getByRole("button", { name: "Retry the apply" }));
    await vi.waitFor(() => expect(fetch.calls.some((r) => r.method === "POST" && r.url.endsWith("/m1/retry"))).toBe(true));
  });

  it("gives no retry for a merge whose target changed or is gone, only the reason", async () => {
    renderMergePage(
      mergeOf({ status: "failed", failed_phase: null, error: "merge_target_gone: the merge's target no longer exists; nothing more is applied", plan }),
    );
    const apply = await screen.findByRole("region", { name: "Apply" });
    expect(within(apply).queryByRole("button", { name: /Retry/ })).toBeNull();
    expect(await screen.findByText("The merge's target is gone.")).toBeInTheDocument();
    expect(screen.getByText("the merge's target no longer exists; nothing more is applied")).toBeInTheDocument();
  });

  it("explains a target that changed under a half applied merge", async () => {
    renderMergePage(
      mergeOf({
        status: "failed",
        failed_phase: null,
        error: "merge_target_changed: the plan's transaction is no longer on the target (it was reset or restored); nothing more is applied (recompute the merge)",
        plan,
      }),
    );
    expect(await screen.findByText("The target changed under the merge.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Recompute" })).toBeEnabled();
  });

  it("abandons a merge that failed half applied, after a confirm, and lists what it left", async () => {
    const leftovers = [
      { kind: "not_valid_constraint", schema: "public", table: "orders", name: "orders_user_fk" },
      { kind: "invalid_index", schema: "public", table: "orders", name: "orders_ix" },
    ];
    const abandoned = mergeOf({ status: "cancelled", failed_phase: null, result: { leftovers }, plan });
    const half = mergeOf({ status: "failed", failed_phase: "online", error: "lock timeout", plan });
    const { fetch } = renderMergePage(half, {
      get: sequence(
        () => jsonResponse(detailOf(half)),
        () => jsonResponse(detailOf(abandoned)),
      ),
      routes: { "POST /v1/databases/db/merges/m1/abandon": () => jsonResponse(abandoned) },
    });
    const apply = await screen.findByRole("region", { name: "Apply" });
    await userEvent.click(within(apply).getByRole("button", { name: "Abandon the merge" }));
    const confirm = within(apply).getByRole("region", { name: "Confirm the abandon" });
    expect(confirm).toHaveTextContent("main keeps everything the plan's transaction changed");
    expect(confirm).toHaveTextContent("Nothing is undone");
    expect(fetch.calls.some((r) => r.url.endsWith("/m1/abandon"))).toBe(false);
    await userEvent.click(within(confirm).getByRole("button", { name: "Confirm abandon" }));
    await vi.waitFor(() => expect(fetch.calls.some((r) => r.method === "POST" && r.url.endsWith("/m1/abandon"))).toBe(true));
    expect(await screen.findByText(/Abandoned half applied/)).toBeInTheDocument();
    const finish = screen.getByRole("region", { name: "Finish" });
    const left = within(finish).getByRole("region", { name: "Left on main" });
    expect(left).toHaveTextContent("orders_user_fk");
    expect(left).toHaveTextContent('ALTER TABLE "public"."orders" VALIDATE CONSTRAINT "orders_user_fk";');
    expect(left).toHaveTextContent('DROP INDEX CONCURRENTLY IF EXISTS "public"."orders_ix";');
  });

  it("can back out of the abandon", async () => {
    const { fetch } = renderMergePage(mergeOf({ status: "failed", failed_phase: "online", error: "lock timeout", plan }));
    const apply = await screen.findByRole("region", { name: "Apply" });
    await userEvent.click(within(apply).getByRole("button", { name: "Abandon the merge" }));
    await userEvent.click(within(apply).getByRole("button", { name: "Keep the merge" }));
    expect(within(apply).queryByRole("region", { name: "Confirm the abandon" })).toBeNull();
    expect(fetch.calls.some((r) => r.url.endsWith("/abandon"))).toBe(false);
  });

  it("says why a half applied merge can't be planned again", async () => {
    renderMergePage(
      mergeOf({
        status: "failed",
        failed_phase: "online",
        error: "merge_half_applied: an earlier apply's transaction is on the target, so the merge isn't computed again; retry it (its online steps run) or abandon it",
        plan,
      }),
    );
    const apply = await screen.findByRole("region", { name: "Apply" });
    expect(within(apply).getByRole("alert")).toHaveTextContent("an earlier apply's transaction is on the target");
    expect(within(apply).getByRole("button", { name: "Retry the online steps" })).toBeEnabled();
    expect(within(apply).getByRole("button", { name: "Abandon the merge" })).toBeEnabled();
  });

  it("retries a merge that failed in its online phase", async () => {
    const { fetch } = renderMergePage(mergeOf({ status: "failed", failed_phase: "online", error: "index build failed", plan }), {
      routes: { "POST /v1/databases/db/merges/m1/retry": () => jsonResponse(mergeOf({ status: "online" }), 202) },
    });
    expect(await screen.findByText(/The plan's transaction is committed: main already changed/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Retry the online steps" }));
    await vi.waitFor(() => expect(fetch.calls.some((r) => r.method === "POST" && r.url.endsWith("/m1/retry"))).toBe(true));
  });
});
