import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { FunctionSecrets } from "./secrets-form";

const BASE = "/v1/databases/d1/branches/b1/functions/hello/secrets";
const VALUE = "sk_live_do-not-show-me-0123456789";

function setup(canChange = true, initial: string[] = ["EXISTING"]) {
  let names = [...initial];
  const f = fakeFetch({
    [`GET ${BASE}`]: () => jsonResponse(names),
    [`PUT ${BASE}/STRIPE_KEY`]: () => {
      names = [...names, "STRIPE_KEY"].sort();
      return new Response(null, { status: 204 });
    },
    [`PUT ${BASE}/EXISTING`]: () => new Response(null, { status: 204 }),
    [`DELETE ${BASE}/EXISTING`]: () => {
      names = names.filter((n) => n !== "EXISTING");
      return new Response(null, { status: 204 });
    },
  });
  const r = renderWithQuery(<FunctionSecrets databaseId="d1" branchId="b1" name="hello" canChange={canChange} />, { fetch: f });
  return { ...r, f };
}

describe("FunctionSecrets", () => {
  it("value_is_never_rendered_after_submit", async () => {
    const user = userEvent.setup();
    const { container, f } = setup();
    await screen.findByText("EXISTING");
    await user.type(screen.getByLabelText("Secret name"), "STRIPE_KEY");
    await user.type(screen.getByLabelText("Value"), VALUE);
    await user.click(screen.getByRole("button", { name: "Save secret" }));
    await screen.findByText("STRIPE_KEY");
    const put = f.calls.find((c) => c.method === "PUT");
    expect(await put!.clone().json()).toEqual({ value: VALUE });
    // The form cleared, and the value is nowhere: not in an input, not in text.
    expect(screen.getByLabelText("Secret name")).toHaveValue("");
    expect(screen.getByLabelText("Value")).toHaveValue("");
    expect(container.innerHTML).not.toContain(VALUE);
    expect(document.body.innerHTML).not.toContain(VALUE);
  });

  it("value_field_is_a_password_field_without_autofill", async () => {
    setup();
    const value = await screen.findByLabelText("Value");
    expect(value).toHaveAttribute("type", "password");
    expect(value).toHaveAttribute("autocomplete", "new-password");
  });

  it("replace_prefills_the_name_never_a_value", async () => {
    const user = userEvent.setup();
    setup();
    await user.click(await screen.findByRole("button", { name: "Replace EXISTING" }));
    expect(screen.getByLabelText("Secret name")).toHaveValue("EXISTING");
    expect(screen.getByLabelText("Value")).toHaveValue("");
  });

  it("refuses_a_bad_name_before_sending", async () => {
    const user = userEvent.setup();
    const { f } = setup();
    await screen.findByText("EXISTING");
    await user.type(screen.getByLabelText("Secret name"), "1BAD");
    await user.type(screen.getByLabelText("Value"), "x");
    expect(screen.getByRole("button", { name: "Save secret" })).toBeDisabled();
    expect(screen.getByText(/letter or _/)).toBeInTheDocument();
    expect(f.calls.some((c) => c.method === "PUT")).toBe(false);
  });

  it("deletes_after_confirming", async () => {
    const user = userEvent.setup();
    setup();
    await user.click(await screen.findByRole("button", { name: "Delete EXISTING" }));
    await user.click(screen.getByRole("button", { name: "Confirm delete" }));
    await waitFor(() => expect(screen.queryByText("EXISTING")).not.toBeInTheDocument());
  });

  it("viewer_sees_names_with_disabled_controls", async () => {
    setup(false);
    await screen.findByText("EXISTING");
    expect(screen.getByLabelText("Secret name")).toBeDisabled();
    expect(screen.getByLabelText("Value")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Delete EXISTING" })).toBeDisabled();
    expect(screen.getAllByText("Viewers can't change secrets. Ask a Developer.").length).toBeGreaterThan(0);
  });
});
