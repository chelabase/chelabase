import { describe, expect, it } from "vitest";
import { member } from "@/test/fake-realtime";
import { avatarLabels, avatarStack, initials } from "./presence";

/**
 * RT1 gf, item 6 (user, 2026-10-21): members of one avatar stack whose initials are the same get a two-letter
 * form that tells them apart.
 *
 * Pinned interface: `avatarLabels(shown: PresenceMember[]): string[]` in presence.ts, one label per member of `shown`
 * (the members drawn in the stack, in order). Rules:
 * - a member whose `initials()` is unique among `shown` keeps `initials()` as today;
 * - members sharing `initials()` are a group; each member of a group gets, in this order of preference, the
 *   first two letters of the display name (spaces dropped, upper-cased) when they are all different, else
 *   first + last initial of the name (upper-cased) when they are all different, else the first two letters
 *   of the email (upper-cased); groups are decided independently of each other;
 * - only the members shown count: a member behind "+N" never makes another's initials ambiguous.
 */

const m = (id: string, name: string | null, email: string | null = `${id}@x.test`) => member({ id, name, email, sub: `s-${id}` });

describe("avatarLabels: members that share initials", () => {
  it("unique_initials_are_kept_as_today", () => {
    const shown = [m("a", "Ana Lee"), m("b", "Bo Kim"), m("c", "Cy", "cy@x.test"), m("d", null, "dee@x.test")];
    expect(avatarLabels(shown)).toEqual(["AL", "BK", "C", "D"]);
    expect(avatarLabels(shown)).toEqual(shown.map(initials));
  });

  it("two_single_word_names_with_the_same_letter_get_their_first_two_letters", () => {
    expect(avatarLabels([m("a", "Rosa"), m("b", "Ravi")])).toEqual(["RO", "RA"]);
  });

  it("the_two_letters_are_upper_case_whatever_the_name_case", () => {
    expect(avatarLabels([m("a", "rosa"), m("b", "Ravi")])).toEqual(["RO", "RA"]);
  });

  it("names_with_the_same_first_two_letters_are_told_apart_by_first_and_last_initial", () => {
    // initials AM and AM; the first two letters are AN and AN; first + last initial are AL and AO
    expect(avatarLabels([m("a", "Ana Maria Lee"), m("b", "Ana Maria Ortiz")])).toEqual(["AL", "AO"]);
  });

  it("names_that_tie_both_ways_fall_back_to_the_first_two_letters_of_the_email", () => {
    // Rosa Diaz / Rosa Dunn: initials RD, first two letters RO, first + last RD: all tie
    expect(avatarLabels([m("a", "Rosa Diaz", "xa@x.test"), m("b", "Rosa Dunn", "yb@x.test")])).toEqual(["XA", "YB"]);
  });

  it("only_the_members_that_collide_change_and_the_order_is_kept", () => {
    const shown = [m("a", "Rosa"), m("b", "Bo Kim"), m("c", "Ravi"), m("d", "Ana Lee")];
    expect(avatarLabels(shown)).toEqual(["RO", "BK", "RA", "AL"]);
  });

  it("three_in_a_group_are_all_told_apart", () => {
    expect(avatarLabels([m("a", "Rosa"), m("b", "Ravi"), m("c", "Remy")])).toEqual(["RO", "RA", "RE"]);
  });

  it("two_groups_are_decided_independently", () => {
    expect(avatarLabels([m("a", "Rosa"), m("b", "Bea"), m("c", "Ravi"), m("d", "Bo")])).toEqual(["RO", "BE", "RA", "BO"]);
  });

  it("an_empty_stack_has_no_labels", () => {
    expect(avatarLabels([])).toEqual([]);
  });

  it("the_labels_of_a_real_stack_use_only_the_five_shown", () => {
    // Rosa is first; Ravi is the sixth, behind "+1": the five shown have unique initials, so Rosa stays "R".
    const members = [m("a", "Rosa"), m("b", "Bo Kim"), m("c", "Cy Lo"), m("d", "Di Ma"), m("e", "Ed Na"), m("f", "Ravi")];
    const { shown, extra } = avatarStack(members);
    expect(extra).toBe(1);
    expect(avatarLabels(shown)).toEqual(["R", "BK", "CL", "DM", "EN"]);
    // ... and when the other R is among the five shown, both change.
    const front = [m("f", "Ravi"), ...members.slice(0, 5)];
    expect(avatarLabels(avatarStack(front).shown).slice(0, 2)).toEqual(["RA", "RO"]);
  });
});
