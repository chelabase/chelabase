import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { apiError, calls, mount, P1, SOFIA, sofiaFor } from "./test-setup";

/** Fix S4b (final recapture, finding 1): a Developer's own member page shows the roles they hold. Pixels are the recapture's. */

const adminOnly = apiError(403, "forbidden", "Admin only");
const own = () =>
  mount({
    caller: "self",
    memberId: SOFIA,
    members: () => [sofiaFor("self")],
    // The grants list is Admin-only: a Developer gets a 403, never the member's grant.
    routes: { [`GET /v1/projects/${P1}/grants`]: adminOnly },
  });

describe("a Developer's own page (fix S4b)", () => {
  it("shows_the_roles_they_hold_checked_and_read_only", async () => {
    own();
    const card = await screen.findByRole("region", { name: "Billing app" });
    for (const name of ["billing_editor", "billing_reader"]) {
      const box = await within(card).findByRole("checkbox", { name });
      expect(box).toBeChecked();
      expect(box).toBeDisabled();
    }
  });

  it("never_asks_the_admin_only_grants_endpoint", async () => {
    const { f } = own();
    const card = await screen.findByRole("region", { name: "Billing app" });
    await within(card).findByRole("checkbox", { name: "billing_editor" });
    expect(calls(f, "GET", `/v1/projects/${P1}/grants`)).toHaveLength(0);
  });
});
