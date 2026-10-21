import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { member } from "@/test/fake-realtime";
import { AvatarStack } from "./presence-layer";

/**
 * RT1 gf, item 6: the stack draws `avatarLabels(shown)` (lib/realtime/gf-presence.test.ts holds the rules): two
 * members of the stack with the same initials show different two-letter forms; the rest are unchanged.
 */
const m = (id: string, name: string | null, email = `${id}@x.test`) => member({ id, name, email, sub: `s-${id}`, color_seed: id.charCodeAt(0) });
const texts = () => screen.getAllByTestId("avatar").map((a) => a.textContent);

describe("AvatarStack: members that share initials", () => {
  it("two_members_with_the_same_initial_show_two_letters_that_differ", () => {
    render(<AvatarStack members={[m("a", "Rosa"), m("b", "Ravi")]} />);
    expect(texts()).toEqual(["RO", "RA"]);
  });

  it("members_with_unique_initials_keep_theirs_beside_a_colliding_pair", () => {
    render(<AvatarStack members={[m("a", "Rosa"), m("b", "Bo Kim"), m("c", "Ravi"), m("d", "Cy", "cy@x.test")]} />);
    expect(texts()).toEqual(["RO", "BK", "RA", "C"]);
  });

  it("a_member_behind_the_plus_n_does_not_make_the_shown_ones_ambiguous", () => {
    const members = [m("a", "Rosa"), m("b", "Bo Kim"), m("c", "Cy Lo"), m("d", "Di Ma"), m("e", "Ed Na"), m("f", "Ravi")];
    render(<AvatarStack members={members} />);
    expect(texts()).toEqual(["R", "BK", "CL", "DM", "EN"]);
    expect(screen.getByText("+1")).toBeInTheDocument();
  });

  it("the_viewers_own_entry_is_not_in_the_stack_so_it_does_not_count_either", () => {
    render(<AvatarStack members={[m("me", "Ravi"), m("a", "Rosa")]} selfSub="s-me" />);
    expect(texts()).toEqual(["R"]);
  });

  it("one_member_with_two_sockets_is_one_avatar_and_not_a_collision_with_itself", () => {
    const rosa = m("a", "Rosa");
    render(<AvatarStack members={[rosa, { ...rosa, id: "a2" }]} />);
    expect(texts()).toEqual(["R"]);
  });

  it("the_accessible_name_still_carries_the_full_display_name", () => {
    render(<AvatarStack members={[m("a", "Rosa"), m("b", "Ravi")]} />);
    expect(screen.getAllByTestId("avatar").map((a) => a.getAttribute("aria-label"))).toEqual(["Rosa, acting as the owner", "Ravi, acting as the owner"]);
  });
});
