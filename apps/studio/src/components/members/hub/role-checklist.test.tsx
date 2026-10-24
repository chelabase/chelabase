import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { BranchRole } from "@chelabase/management-client";
import { roleChoices, type RoleChoice } from "../role-picker";
import { escapeName, LevelBadge, RoleChecklist, ViewToggle } from "./index";

function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false,
    ...extra,
  } as BranchRole;
}

const choice = (value: string, extra: Partial<RoleChoice> = {}): RoleChoice => ({ value, chain: [], ...extra });
const many = (n: number) => Array.from({ length: n }, (_, i) => choice(`role_${String(i).padStart(2, "0")}`));

describe("RoleChecklist", () => {
  it("role_checklist_filters_above_8_and_shows_also_gives", async () => {
    const eight = render(<RoleChecklist choices={many(8)} checked={[]} onChange={() => {}} />);
    expect(screen.queryByRole("searchbox")).toBeNull();
    expect(screen.getAllByRole("checkbox")).toHaveLength(8);
    eight.unmount();

    const roles = [role("billing_reader"), role("billing_editor", { member_of: ["billing_reader"] }), ...many(8).map((c) => role(c.value))];
    const choices = roleChoices([{ database: "orders", roles }], "developer");
    expect(choices).toHaveLength(10);
    render(<RoleChecklist choices={choices} checked={[]} onChange={() => {}} />);
    const filter = screen.getByRole("searchbox");
    // The chain is said next to the role that brings it.
    expect(screen.getByText(/also gives billing_reader/)).toBeInTheDocument();
    await userEvent.type(filter, "billing");
    expect(screen.getAllByRole("checkbox").map((c) => c.getAttribute("aria-label") ?? c.textContent)).toEqual(
      expect.arrayContaining([expect.stringContaining("billing_reader"), expect.stringContaining("billing_editor")]),
    );
    expect(screen.getAllByRole("checkbox")).toHaveLength(2);
    await userEvent.clear(filter);
    await userEvent.type(filter, "zzz");
    expect(screen.queryAllByRole("checkbox")).toHaveLength(0);
  });

  it("lists checked roles first and keeps the rest in order", () => {
    const choices = [choice("a"), choice("b"), choice("c"), choice("d")];
    render(<RoleChecklist choices={choices} checked={["c", "b"]} onChange={() => {}} />);
    const boxes = ["b", "c", "a", "d"].map((n) => screen.getByRole("checkbox", { name: n }));
    // Document order: checked roles first (in the choices' order), then the rest.
    for (let i = 1; i < boxes.length; i++) {
      expect(boxes[i - 1].compareDocumentPosition(boxes[i]) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    }
    expect(boxes[0]).toBeChecked();
    expect(boxes[1]).toBeChecked();
    expect(boxes[2]).not.toBeChecked();
    expect(boxes[3]).not.toBeChecked();
  });

  it("toggles a role through onChange with raw names and keeps Admin-only roles disabled with their reason", async () => {
    const onChange = vi.fn();
    const choices = [choice("a"), choice("b"), choice("migrator", { disabledReason: "Admin only: LOGIN role" })];
    render(<RoleChecklist choices={choices} checked={["a"]} onChange={onChange} />);
    await userEvent.click(screen.getByRole("checkbox", { name: "b" }));
    expect(onChange).toHaveBeenLastCalledWith(["a", "b"]);
    await userEvent.click(screen.getByRole("checkbox", { name: "a" }));
    expect(onChange).toHaveBeenLastCalledWith([]);
    const blocked = screen.getByRole("checkbox", { name: "migrator" });
    expect(blocked).toBeDisabled();
    expect(screen.getByText(/LOGIN role/)).toBeInTheDocument();
    onChange.mockClear();
    await userEvent.click(blocked);
    expect(onChange).not.toHaveBeenCalled();
  });

  it("shows a locked team role checked and disabled, and never puts it in onChange", async () => {
    const onChange = vi.fn();
    const choices = [choice("own_a"), choice("own_b"), choice("support_agent")];
    render(
      <RoleChecklist choices={choices} checked={["own_a"]} locked={[{ value: "support_agent", via: "Support team" }]} onChange={onChange} />,
    );
    const locked = screen.getByRole("checkbox", { name: "support_agent" });
    expect(locked).toBeDisabled();
    expect(locked).toBeChecked();
    expect(screen.getByText(/via Support team/)).toBeInTheDocument();
    await userEvent.click(locked);
    expect(onChange).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("checkbox", { name: "own_b" }));
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange.mock.calls[0][0]).toEqual(["own_a", "own_b"]);
    await userEvent.click(screen.getByRole("checkbox", { name: "own_a" }));
    expect(onChange).toHaveBeenLastCalledWith([]);
  });

  it("role_checklist_escapes_control_and_bidi_characters", async () => {
    const onChange = vi.fn();
    const bidi = "refunds‮review";
    const choices = [choice(bidi), choice("tab\tname"), choice("<img src=x onerror=alert(1)>"), choice("café")];
    const { container } = render(<RoleChecklist choices={choices} checked={[]} onChange={onChange} />);
    // The visible text is the escaped form, never the raw character.
    expect(container.textContent).not.toContain("‮");
    expect(container.textContent).not.toContain("\t");
    expect(screen.getByText(/refunds\\u202e/i)).toBeInTheDocument();
    expect(screen.getByText(/tab\\u0009name/i)).toBeInTheDocument();
    expect(screen.getByText(/caf\\u00e9/i)).toBeInTheDocument();
    // Markup in a name is text.
    expect(container.querySelector("img")).toBeNull();
    expect(screen.getByText("<img src=x onerror=alert(1)>")).toBeInTheDocument();
    // The value passed on is the raw name.
    await userEvent.click(screen.getByRole("checkbox", { name: /refunds/ }));
    expect(onChange).toHaveBeenCalledWith([bidi]);
  });
});

describe("escapeName", () => {
  it("shows non-ASCII and control characters as \\uXXXX and leaves printable ASCII alone", () => {
    expect(escapeName("billing_reader-2")).toBe("billing_reader-2");
    expect(escapeName("refunds‮review").toLowerCase()).toBe("refunds\\u202ereview");
    expect(escapeName("a\nb").toLowerCase()).toBe("a\\u000ab");
    expect(escapeName("a\u007Fb").toLowerCase()).toBe("a\\u007fb");
    expect(escapeName("café").toLowerCase()).toBe("caf\\u00e9");
    // Zero-width and look-alike characters are not invisible.
    expect(escapeName("ad​min").toLowerCase()).toBe("ad\\u200bmin");
    expect(escapeName("аdmin").toLowerCase()).toBe("\\u0430dmin");
    expect(escapeName("")).toBe("");
  });
});

describe("LevelBadge", () => {
  it("names each level", () => {
    const { container } = render(
      <div>
        <LevelBadge level="tenant_admin" />
        <LevelBadge level="admin" />
        <LevelBadge level="developer" />
        <LevelBadge level="viewer" />
      </div>,
    );
    expect(container.textContent).toBe("Tenant adminProject adminDeveloperViewer");
  });
});

describe("ViewToggle", () => {
  it("is a radio group of Cards and List that reports the choice", async () => {
    const onChange = vi.fn();
    render(<ViewToggle value="list" onChange={onChange} />);
    const group = screen.getByRole("radiogroup", { name: "View" });
    expect(within(group).getByRole("radio", { name: "List" })).toBeChecked();
    expect(within(group).getByRole("radio", { name: "Cards" })).not.toBeChecked();
    await userEvent.click(within(group).getByRole("radio", { name: "Cards" }));
    expect(onChange).toHaveBeenCalledWith("cards");
  });
});
