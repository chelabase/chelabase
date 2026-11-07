import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { ProjectCapsContext, projectCaps } from "@/lib/capabilities";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { meAs, type Level } from "@/test/table-editor-fixtures";
import { DEV_COMMAND, FunctionList } from "./function-list";

const fn = (name: string, over: Record<string, unknown> = {}) => ({
  id: `id-${name}`, name, kind: "function", auth: "key", enabled: false, deployment_id: "dep-new-0000", last_error: null,
  run_as_role: null, run_as_active: false, created_at: "2026-11-01T00:00:00Z", updated_at: null, ...over,
});
const DEPLOYMENTS = [
  { id: "dep-new-0000", note: "second", created_at: "2026-11-06T00:00:00Z", created_by: "m1", compatibility_date: "2026-10-01", bundle_sha256: null, bundle_size: null },
  { id: "dep-old-0000", note: "first", created_at: "2026-11-05T00:00:00Z", created_by: "m1", compatibility_date: "2026-10-01", bundle_sha256: null, bundle_size: null },
];

function base(b: string) {
  return `/v1/databases/d1/branches/${b}/functions`;
}

function setup(level: Level, routes: Record<string, FakeHandler>, branchId = "b2") {
  const f = fakeFetch({
    [`GET ${base(branchId)}/hello/deployments`]: () => jsonResponse(DEPLOYMENTS),
    [`GET ${base(branchId)}/hello/secrets`]: () => jsonResponse([]),
    ...routes,
  });
  const caps = projectCaps(meAs(level) as never, "p1", "d1");
  const ui = (b: string) => (
    <ProjectCapsContext.Provider value={caps}>
      <FunctionList key={b} databaseId="d1" branchId={b} apiUrl="http://api.test" />
    </ProjectCapsContext.Provider>
  );
  const r = renderWithQuery(ui(branchId), { fetch: f });
  return { ...r, f, ui };
}

describe("FunctionList", () => {
  it("switch_reverts_and_says_why_when_the_api_refuses", async () => {
    const user = userEvent.setup();
    let release: (r: Response) => void = () => {};
    setup("developer", {
      [`GET ${base("b2")}`]: () => jsonResponse([fn("hello")]),
      [`PATCH ${base("b2")}/hello`]: () => new Promise<Response>((r) => (release = r)),
    });
    const sw = await screen.findByRole("switch", { name: "hello enabled" });
    expect(sw).not.toBeChecked();
    await user.click(sw);
    // At once, before the API answers.
    expect(sw).toBeChecked();
    release(jsonResponse({ error: { code: "conflict", message: "raw", reason: "function_active_on_protected_branch" } }, 409));
    await waitFor(() => expect(sw).not.toBeChecked());
    expect(screen.getByRole("alert")).toHaveTextContent(/protected branch/);
  });

  it("switch_stays_on_after_success", async () => {
    const user = userEvent.setup();
    let enabled = false;
    setup("developer", {
      [`GET ${base("b2")}`]: () => jsonResponse([fn("hello", { enabled })]),
      [`PATCH ${base("b2")}/hello`]: () => {
        enabled = true;
        return jsonResponse(fn("hello", { enabled }));
      },
    });
    const sw = await screen.findByRole("switch", { name: "hello enabled" });
    await user.click(sw);
    await waitFor(() => expect(sw).not.toBeDisabled());
    expect(sw).toBeChecked();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("make_active_rolls_back_to_the_older_deployment", async () => {
    const user = userEvent.setup();
    let active = "dep-new-0000";
    const sent: unknown[] = [];
    setup("developer", {
      [`GET ${base("b2")}`]: () => jsonResponse([fn("hello", { deployment_id: active })]),
      [`PATCH ${base("b2")}/hello`]: async (req) => {
        const body = (await req.json()) as { active_deployment_id: string };
        sent.push(body);
        active = body.active_deployment_id;
        return jsonResponse(fn("hello", { deployment_id: active }));
      },
    });
    await user.click(await screen.findByRole("button", { name: "hello" }));
    await user.click(await screen.findByRole("button", { name: "Make dep-old- active" }));
    await screen.findByRole("button", { name: "Make dep-new- active" });
    expect(sent).toEqual([{ active_deployment_id: "dep-old-0000" }]);
  });

  it("viewer_gets_disabled_controls", async () => {
    const user = userEvent.setup();
    setup("viewer", { [`GET ${base("b2")}`]: () => jsonResponse([fn("hello")]) });
    expect(await screen.findByRole("switch", { name: "hello enabled" })).toHaveAttribute("data-disabled");
    await user.click(screen.getByRole("button", { name: "hello" }));
    expect(await screen.findByRole("button", { name: "Make dep-old- active" })).toBeDisabled();
    expect(screen.getByLabelText("Run as role")).toBeDisabled();
  });

  it("run_as_refusal_shows_the_api_message", async () => {
    const user = userEvent.setup();
    setup("developer", {
      [`GET ${base("b2")}`]: () => jsonResponse([fn("hello")]),
      [`PATCH ${base("b2")}/hello`]: () => jsonResponse({ error: { code: "forbidden", message: "a role with BYPASSRLS needs Admin" } }, 403),
    });
    await user.click(await screen.findByRole("button", { name: "hello" }));
    await user.type(await screen.findByLabelText("Run as role"), "bypasser");
    await user.click(screen.getByRole("button", { name: "Set role" }));
    expect(await screen.findByText("a role with BYPASSRLS needs Admin")).toBeInTheDocument();
  });

  it("branch_change_drops_a_pending_switch", async () => {
    const user = userEvent.setup();
    const { rerender, ui } = setup("developer", {
      [`GET ${base("b2")}`]: () => jsonResponse([fn("hello")]),
      [`PATCH ${base("b2")}/hello`]: () => new Promise<Response>(() => {}),
      [`GET ${base("b3")}`]: () => jsonResponse([fn("hello")]),
    });
    await user.click(await screen.findByRole("switch", { name: "hello enabled" }));
    expect(screen.getByRole("switch", { name: "hello enabled" })).toBeChecked();
    rerender(ui("b3"));
    await waitFor(() => expect(screen.getByRole("switch", { name: "hello enabled" })).not.toBeChecked());
    expect(screen.getByRole("switch", { name: "hello enabled" })).not.toHaveAttribute("data-disabled");
  });

  it("empty_state_shows_the_dev_command", async () => {
    setup("developer", { [`GET ${base("b2")}`]: () => jsonResponse([]) });
    expect(await screen.findByText(DEV_COMMAND)).toBeInTheDocument();
    expect(DEV_COMMAND).toBe("chelabase functions dev --watch");
  });
});
