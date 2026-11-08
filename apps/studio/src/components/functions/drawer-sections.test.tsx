import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { describe, expect, it } from "vitest";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { SchedulesSection } from "./schedules-section";
import { AWAKE_NOTE, TriggersSection } from "./triggers-section";

const F = "/v1/databases/d1/branches/b1/functions/hello";
const SCHEDULE = { id: "s1", cron: "0 9 * * *", timezone: "UTC", enabled: true, next_run_at: null };
const TRIGGER = {
  id: "t1", source: "table", pg_database: "app", schema: "public", table: "orders", bucket: null, prefix: null, events: ["insert"],
  enabled: true, state: "installed", last_error: null, last_delivery_error: null, dead_count: 2, dropped_count: 0, last_delivered_at: null,
};

function render(ui: ReactElement, routes: Record<string, FakeHandler>) {
  const f = fakeFetch({
    "GET /v1/databases/d1/branches/b1/databases": () => jsonResponse([{ name: "app", main: true, data_api: "on", data_api_error: null, default_schema: "public", origin: "main", owner: "o" }]),
    ...routes,
  });
  return { ...renderWithQuery(ui, { fetch: f }), f };
}

describe("SchedulesSection", () => {
  it("adds_a_schedule_and_clears_the_form", async () => {
    const user = userEvent.setup();
    let schedules: unknown[] = [];
    const sent: unknown[] = [];
    render(<SchedulesSection databaseId="d1" branchId="b1" name="hello" canChange />, {
      [`GET ${F}/schedules`]: () => jsonResponse(schedules),
      [`POST ${F}/schedules`]: async (req) => {
        sent.push(await req.json());
        schedules = [{ ...SCHEDULE, id: "s2", cron: "*/5 * * * *", timezone: "Europe/London" }];
        return jsonResponse(schedules[0], 201);
      },
    });
    await screen.findByText(/No schedules/);
    await user.type(screen.getByLabelText("Cron"), "*/5 * * * *");
    await user.clear(screen.getByLabelText("Time zone"));
    await user.type(screen.getByLabelText("Time zone"), "Europe/London");
    await user.click(screen.getByRole("button", { name: "Add schedule" }));
    await screen.findByText("*/5 * * * *");
    expect(sent).toEqual([{ cron: "*/5 * * * *", timezone: "Europe/London" }]);
    expect(screen.getByLabelText("Cron")).toHaveValue("");
  });

  it("blocks_an_obviously_bad_cron", async () => {
    const user = userEvent.setup();
    render(<SchedulesSection databaseId="d1" branchId="b1" name="hello" canChange />, { [`GET ${F}/schedules`]: () => jsonResponse([]) });
    await screen.findByText(/No schedules/);
    await user.type(screen.getByLabelText("Cron"), "61 * * * *");
    expect(screen.getByRole("button", { name: "Add schedule" })).toBeDisabled();
    expect(screen.getByText(/minute field/)).toBeInTheDocument();
  });

  it("edit_sends_a_patch", async () => {
    const user = userEvent.setup();
    const sent: unknown[] = [];
    render(<SchedulesSection databaseId="d1" branchId="b1" name="hello" canChange />, {
      [`GET ${F}/schedules`]: () => jsonResponse([SCHEDULE]),
      [`PATCH ${F}/schedules/s1`]: async (req) => {
        sent.push(await req.json());
        return jsonResponse(SCHEDULE);
      },
    });
    await user.click(await screen.findByRole("button", { name: "Edit schedule 0 9 * * *" }));
    expect(screen.getByLabelText("Cron")).toHaveValue("0 9 * * *");
    await user.clear(screen.getByLabelText("Cron"));
    await user.type(screen.getByLabelText("Cron"), "0 10 * * *");
    await user.click(screen.getByRole("button", { name: "Save schedule" }));
    await waitFor(() => expect(sent).toEqual([{ cron: "0 10 * * *", timezone: "UTC" }]));
  });

  it("shows_the_api_refusal", async () => {
    const user = userEvent.setup();
    render(<SchedulesSection databaseId="d1" branchId="b1" name="hello" canChange />, {
      [`GET ${F}/schedules`]: () => jsonResponse([]),
      [`POST ${F}/schedules`]: () => jsonResponse({ error: { code: "invalid_request", message: "a service can't have schedules" } }, 422),
    });
    await screen.findByText(/No schedules/);
    await user.type(screen.getByLabelText("Cron"), "* * * * *");
    await user.click(screen.getByRole("button", { name: "Add schedule" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("a service can't have schedules");
  });

  it("viewer_is_read_only", async () => {
    render(<SchedulesSection databaseId="d1" branchId="b1" name="hello" canChange={false} />, { [`GET ${F}/schedules`]: () => jsonResponse([SCHEDULE]) });
    expect(await screen.findByRole("switch", { name: "Schedule 0 9 * * * enabled" })).toHaveAttribute("data-disabled");
    expect(screen.getByRole("button", { name: "Delete schedule 0 9 * * *" })).toBeDisabled();
    expect(screen.getByLabelText("Cron")).toBeDisabled();
  });
});

describe("TriggersSection", () => {
  it("lists_with_counts_and_the_awake_note", async () => {
    render(<TriggersSection databaseId="d1" branchId="b1" name="hello" canChange />, { [`GET ${F}/triggers`]: () => jsonResponse([TRIGGER]) });
    expect(await screen.findByText("app · public.orders")).toBeInTheDocument();
    expect(screen.getByText("installed")).toBeInTheDocument();
    expect(screen.getByText("2 dead")).toBeInTheDocument();
    expect(screen.getByText(AWAKE_NOTE)).toBeInTheDocument();
  });

  it("adds_a_table_trigger_and_shows_a_422", async () => {
    const user = userEvent.setup();
    const sent: unknown[] = [];
    render(<TriggersSection databaseId="d1" branchId="b1" name="hello" canChange />, {
      [`GET ${F}/triggers`]: () => jsonResponse([]),
      [`POST ${F}/triggers`]: async (req) => {
        sent.push(await req.json());
        return jsonResponse({ error: { code: "invalid_request", message: "the schema auth is reserved" } }, 422);
      },
    });
    await screen.findByText(/No triggers/);
    await waitFor(() => expect(screen.getByLabelText("Postgres database")).toHaveValue("app"));
    await user.type(screen.getByLabelText("Table"), "auth.users");
    await user.click(screen.getByRole("button", { name: "Add trigger" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("the schema auth is reserved");
    expect(sent).toEqual([{ source: "table", pg_database: "app", schema: "auth", table: "users", events: ["insert", "update", "delete"] }]);
  });

  it("adds_a_storage_trigger_without_blank_filters", async () => {
    const user = userEvent.setup();
    const sent: unknown[] = [];
    render(<TriggersSection databaseId="d1" branchId="b1" name="hello" canChange />, {
      [`GET ${F}/triggers`]: () => jsonResponse([]),
      [`POST ${F}/triggers`]: async (req) => {
        sent.push(await req.json());
        return jsonResponse({ ...TRIGGER, source: "storage" }, 201);
      },
    });
    await screen.findByText(/No triggers/);
    await waitFor(() => expect(screen.getByLabelText("Postgres database")).toHaveValue("app"));
    await user.click(screen.getByRole("radio", { name: "Storage" }));
    await user.click(screen.getByRole("checkbox", { name: "object.moved" }));
    await user.click(screen.getByRole("button", { name: "Add trigger" }));
    await waitFor(() => expect(sent).toEqual([{ source: "storage", pg_database: "app", events: ["object.created", "object.updated", "object.deleted"] }]));
  });

  it("switch_reverts_on_refusal", async () => {
    const user = userEvent.setup();
    render(<TriggersSection databaseId="d1" branchId="b1" name="hello" canChange />, {
      [`GET ${F}/triggers`]: () => jsonResponse([TRIGGER]),
      [`PATCH ${F}/triggers/t1`]: () => jsonResponse({ error: { code: "forbidden", message: "needs admin on a protected branch" } }, 403),
    });
    const sw = await screen.findByRole("switch", { name: "Trigger on app · public.orders enabled" });
    await user.click(sw);
    await waitFor(() => expect(sw).toBeChecked());
    expect(await screen.findByRole("alert")).toHaveTextContent("needs admin on a protected branch");
  });

  it("viewer_is_read_only", async () => {
    render(<TriggersSection databaseId="d1" branchId="b1" name="hello" canChange={false} />, { [`GET ${F}/triggers`]: () => jsonResponse([TRIGGER]) });
    expect(await screen.findByRole("switch", { name: "Trigger on app · public.orders enabled" })).toHaveAttribute("data-disabled");
    expect(screen.getByRole("button", { name: "Delete trigger on app · public.orders" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Add trigger" })).toBeDisabled();
  });
});
