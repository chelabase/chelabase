import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { mountTab } from "./test-setup";

/** Fix S3 (finish review round 2, item 8): the Roles list keeps the member count where the column is hidden. */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

describe("Roles list at 390 px", () => {
  it("says_the_member_count_under_the_name_when_the_members_column_is_hidden", async () => {
    mountTab({}, "/app/members?tab=roles&view=list");
    const table = await screen.findByRole("table", { name: "Roles" });
    const link = await within(table).findByRole("link", { name: /^billing_editor\b/ });
    const counts = within(link).getAllByText(/2 members/);
    // One copy under the name, shown only below sm...
    expect(counts.some((el) => /(^|\s)sm:hidden(\s|$)/.test(el.className) && /^\s*2 members/.test(el.textContent ?? ""))).toBe(true);
    // ...and the Members column, hidden below sm.
    expect(counts.some((el) => /(^|\s)(max-sm:hidden|hidden)(\s|$)/.test(el.className) && /sm:/.test(el.className))).toBe(true);
  });
});
