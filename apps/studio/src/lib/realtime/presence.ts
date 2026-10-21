import type { PresenceMember, Key } from "./types";

/**
 * The 8 presence hues; a member's is `PRESENCE_COLORS[color_seed % 8]`. They are presence-only (the last two are
 * hex literals, not theme tokens) and checked for contrast and distinctness on both themes (Basalt and Lagoon); see
 * presence-palette.test.ts. The viewer's own selection outline uses the viewer's own presence colour; sand is only its
 * fallback without realtime (no presence), and coral stays hazard, so neither sand nor coral is a presence colour.
 */
export const PRESENCE_COLORS: readonly string[] = [
  "var(--seafoam)",
  "var(--method-get)",
  "var(--palm)",
  "var(--method-put)",
  "var(--method-patch)",
  "var(--shell)",
  "#e08ad6", // orchid pink (magenta family)
  "#2498fc", // clear blue (OKLCH hue ~250), tuned for Basalt: >= 23 from seafoam, >= 18 from --method-get and every other hue; its Lagoon tints are slightly under the floor (user ruling 2026-10-21)
];
export const MAX_AVATARS = 5;

export function presenceColorIndex(seed: number): number {
  return ((Math.trunc(seed) % PRESENCE_COLORS.length) + PRESENCE_COLORS.length) % PRESENCE_COLORS.length;
}
export function presenceColor(seed: number): string {
  return PRESENCE_COLORS[presenceColorIndex(seed)];
}

/** `presence_state` replaces the list; `presence_diff` applies leaves then joins by `id` (an update is a leave plus a join of one `id`). */
export function applyPresenceDiff(members: PresenceMember[], diff: { joins: PresenceMember[]; leaves: PresenceMember[] }): PresenceMember[] {
  const gone = new Set([...diff.leaves, ...diff.joins].map((m) => m.id));
  return [...members.filter((m) => !gone.has(m.id)), ...diff.joins];
}

/** Members whose selection is drawn: not the viewer's own (`sub`), with a row. */
export function drawnMembers(members: PresenceMember[], selfSub: string): PresenceMember[] {
  return members.filter((m) => m.sub !== selfSub && m.state.row !== null);
}

/** One entry per member (`sub`), however many sockets they have, in first-seen order; the viewer (`selfSub`) left out. */
export function people(members: PresenceMember[], selfSub: string): PresenceMember[] {
  const seen = new Set<string>();
  const out: PresenceMember[] = [];
  for (const m of members) {
    if ((selfSub !== "" && m.sub === selfSub) || seen.has(m.sub)) continue;
    seen.add(m.sub);
    out.push(m);
  }
  return out;
}

/** The avatar stack: at most 5 members, then the count of the rest. */
export function avatarStack(members: PresenceMember[]): { shown: PresenceMember[]; extra: number } {
  return { shown: members.slice(0, MAX_AVATARS), extra: Math.max(0, members.length - MAX_AVATARS) };
}

/** "AL" for "Ana Lee", "A" for "Ana", the email's first letter without a name. */
export function initials(m: Pick<PresenceMember, "name" | "email">): string {
  const words = (m.name ?? "").trim().split(/\s+/).filter(Boolean);
  if (words.length > 0) return words.slice(0, 2).map((w) => [...w][0].toUpperCase()).join("");
  const email = (m.email ?? "").trim();
  return email ? [...email][0].toUpperCase() : "?";
}

/** The first `n` letters of `text`, spaces dropped, upper-cased. */
const leading = (text: string, n: number) => [...text.replace(/\s+/g, "")].slice(0, n).join("").toUpperCase();

/** First and last word's initials of the member's name (else of the display name). */
function firstLast(m: Pick<PresenceMember, "name" | "email">): string {
  const words = (m.name?.trim() || displayName(m)).split(/\s+/).filter(Boolean);
  if (words.length === 0) return "";
  return [[...words[0]][0], [...words[words.length - 1]][0]].join("").toUpperCase();
}

/** Whether every label in the list is different. */
const allDistinct = (labels: string[]) => new Set(labels).size === labels.length;

/**
 * One label per shown member: `initials()` when unique among `shown`; members sharing initials get, as a group, the first
 * two letters of the display name, else first and last initial of the name, else the first two letters of the email
 * (the first form that tells the whole group apart). Only `shown` counts: members behind "+N" never make one ambiguous.
 */
export function avatarLabels(shown: PresenceMember[]): string[] {
  const labels = shown.map(initials);
  const groups = new Map<string, number[]>();
  labels.forEach((l, i) => groups.set(l, [...(groups.get(l) ?? []), i]));
  for (const idx of groups.values()) {
    if (idx.length < 2) continue;
    const group = idx.map((i) => shown[i]);
    const forms = [
      group.map((m) => leading(displayName(m), 2)),
      group.map(firstLast),
      group.map((m) => leading(m.email ?? "", 2)),
    ];
    const chosen = forms.find(allDistinct) ?? forms[2];
    idx.forEach((i, j) => (labels[i] = chosen[j] || labels[i]));
  }
  return labels;
}

/** How a member is named in text: the name, else the email, else "Someone". */
export function displayName(m: Pick<PresenceMember, "name" | "email">): string {
  return m.name?.trim() || m.email?.trim() || "Someone";
}

/** "Ana is editing this cell" (client side, from another member's `editing: true` on this row and column); null when nobody. */
export function editingNotice(members: PresenceMember[], selfSub: string, row: Key, column: string): string | null {
  const names = [
    ...new Set(
      members.filter((m) => m.sub !== selfSub && m.state.editing && m.state.row === row && m.state.column === column).map(displayName),
    ),
  ];
  if (names.length === 0) return null;
  if (names.length === 1) return `${names[0]} is editing this cell`;
  return `${names.slice(0, -1).join(", ")} and ${names.at(-1)} are editing this cell`;
}
