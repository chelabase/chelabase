import { screen } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import type { AccessEvent } from "@chelabase/management-client";
import { person } from "../hub/fixtures";
import { MARA, P1, P2, SOFIA, mount, sofiaFor } from "./test-setup";

/** Fix S5 (Impeccable review 3, polish 4 and 8): the suspend banner and the Activity rows below sm. */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const tokens = (el: Element) => el.className.split(/\s+/);
const mara = person(MARA, { name: "Mara Okafor", level: "tenant_admin", projects: [[P1, "admin"], [P2, "admin"]] });

describe("the suspend banner (fix S5)", () => {
  async function banner() {
    mount({
      caller: "tenant_admin",
      members: () => [sofiaFor("tenant_admin", { status: "suspended", suspended_at: "2026-10-14T09:00:00Z", suspended_by: MARA }), mara],
    });
    const lead = await screen.findByText(/Suspended on 14 Oct 2026 by Mara Okafor/);
    return { lead, box: lead.closest("div")! };
  }

  it("the_marker_is_a_hollow_square_border_and_no_fill", async () => {
    const { box } = await banner();
    const marker = box.querySelector<HTMLElement>(':scope > span[aria-hidden="true"]')!;
    expect(marker).not.toBeNull();
    expect(tokens(marker)).toContain("border");
    expect(tokens(marker).some((c) => c.startsWith("bg-"))).toBe(false);
  });

  it("the_bold_lead_in_and_the_first_sentence_share_one_line", async () => {
    const { lead } = await banner();
    // The lead-in is inline and its paragraph carries the first sentence too, so they wrap as one text.
    expect(lead.tagName).toBe("STRONG");
    expect(lead.parentElement!.tagName).toBe("P");
    expect(lead.parentElement!).toHaveTextContent(/Suspended on 14 Oct 2026 by Mara Okafor\.\s*Sofia can't sign in to Studio/);
  });
});

describe("Activity rows below sm (fix S5)", () => {
  const event: AccessEvent = {
    id: 7, action: "admin_set", at: "2026-10-20T10:00:00Z", actor_member_id: MARA, actor_email: "mara@northwind.dev",
    target_member_id: SOFIA, target_email: "sofia@northwind.dev", project_id: null, project_name: null, database_id: null, branch_id: null,
    before: null, after: null,
  };

  it("the_time_stacks_above_the_event_below_sm_and_sits_beside_it_from_sm", async () => {
    mount({ caller: "tenant_admin", events: [event] });
    const sentence = await screen.findByText("Mara Okafor made Sofia Marchetti a tenant admin");
    const time = sentence.closest("li")!.querySelector("time")!;
    // The nearest element holding both the time and the sentence is the row's layout box.
    let box: HTMLElement = time.parentElement!;
    while (!box.contains(sentence)) box = box.parentElement!;
    expect(tokens(box)).toContain("flex-col");
    expect(tokens(box)).toContain("sm:flex-row");
  });
});
