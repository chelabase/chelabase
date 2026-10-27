import { useId, useState } from "react";
import { Lock, Search } from "lucide-react";
import { Checkbox } from "@/components/table-editor/panel-bits";
import type { RoleChoice } from "../role-picker";
import { escapeName } from "./escape-name";
import { joinAnd, TeamsPhrase, type TeamRef } from "./team-via";

export interface RoleChecklistProps {
  choices: readonly RoleChoice[];
  /** The roles of the member's own grant (never a locked one). */
  checked: readonly string[];
  /** The roles as last saved: a row whose box differs from it is marked changed. */
  baseline?: readonly string[];
  /**
   * Roles teams give (step 2), with every team that names each. A box always shows the member's own grant
   * (D1), so a team role is a marker on the right ("✓ via the X team", "also via …" when the own grant names it
   * too) and never part of `onChange`'s result. With `lockedRows` a role only a team gives shows checked and
   * locked instead.
   */
  locked?: readonly { value: string; teams: readonly TeamRef[] }[];
  lockedRows?: boolean;
  /** Checked roles that are not allowed (Admin-only below Admin): their rows are marked. */
  invalid?: readonly string[];
  onChange: (next: string[]) => void;
  disabled?: boolean;
}

/** The roles of `checked` that are Admin-only while the level is below Admin. */
export function adminOnlyChecked(choices: readonly RoleChoice[], checked: readonly string[]): string[] {
  return checked.filter((r) => choices.find((c) => c.value === r)?.disabledReason);
}

/** Names the Admin-only roles a grant holds below Admin and what to do about them. */
export function AdminOnlyNote({ names }: { names: readonly string[] }) {
  return (
    <p className="text-xs text-coral">
      {joinAnd(names, (n) => <span className="font-mono text-[12px]">{escapeName(n)}</span>, (n) => n)}{" "}
      {names.length === 1 ? "is" : "are"} Admin-only. Untick {names.length === 1 ? "it" : "them"}, or set the level to Admin.
    </p>
  );
}

/** Above this many roles a filter field appears. */
const FILTER_ABOVE = 8;

/**
 * A role list as checkboxes, checked roles first. Names are shown escaped (MH-38d) and passed on raw;
 * a role that brings others says "also gives …"; a role only an Admin may hold is disabled with its
 * reason; a team's role is checked, disabled and says where it comes from.
 */
export function RoleChecklist({ choices, checked, baseline, locked = [], lockedRows = false, invalid = [], onChange, disabled }: RoleChecklistProps) {
  const filterId = useId();
  const [query, setQueryState] = useState("");
  const lockedBy = new Map(locked.map((l) => [l.value, l.teams]));
  const isOn = (v: string) => checked.includes(v) || (lockedRows && lockedBy.has(v));
  // Checked roles first, sorted at mount and when the filter changes; clicking a box never moves a row.
  const [first, setFirst] = useState<ReadonlySet<string>>(() => new Set([...checked, ...lockedBy.keys()]));
  function setQuery(next: string) {
    setQueryState(next);
    setFirst(new Set([...checked, ...lockedBy.keys()]));
  }
  const q = query.trim().toLowerCase();
  const matching = choices.filter((c) => q === "" || c.value.toLowerCase().includes(q) || escapeName(c.value).toLowerCase().includes(q));
  const ordered = [...matching.filter((c) => first.has(c.value)), ...matching.filter((c) => !first.has(c.value))];

  function toggle(value: string) {
    onChange(checked.includes(value) ? checked.filter((v) => v !== value) : [...checked, value]);
  }

  return (
    <div className="space-y-2">
      {choices.length > FILTER_ABOVE && (
        <div className="relative">
          <label htmlFor={filterId} className="sr-only">
            {`Filter ${choices.length} roles`}
          </label>
          <Search aria-hidden="true" size={14} strokeWidth={1.5} className="pointer-events-none absolute top-1/2 left-2 -translate-y-1/2 text-chalk-dim" />
          <input
            id={filterId}
            type="search"
            value={query}
            placeholder={`Filter ${choices.length} roles`}
            onChange={(e) => setQuery(e.target.value)}
            className="h-8 w-full border border-input bg-ink pr-2 pl-7 text-sm text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
          />
        </div>
      )}
      <ul className="divide-y divide-line border border-line">
        {ordered.map((c) => {
          const via = lockedBy.get(c.value);
          const blocked = c.disabledReason !== undefined && !checked.includes(c.value);
          const own = checked.includes(c.value);
          const lockedRow = lockedRows && via !== undefined && !own;
          const off = disabled || lockedRow || blocked;
          return (
            <li
              key={c.value}
              data-changed={baseline !== undefined && baseline.includes(c.value) !== checked.includes(c.value) ? "true" : undefined}
              data-invalid={invalid.includes(c.value) && checked.includes(c.value) ? "true" : undefined}
              className="flex flex-wrap items-start gap-x-2 gap-y-0.5 px-2 py-1.5 data-[changed=true]:shadow-[inset_2px_0_0_var(--sand)] data-[invalid=true]:shadow-[inset_2px_0_0_var(--coral)]"
            >
              <Checkbox
                aria-label={escapeName(c.value)}
                checked={isOn(c.value)}
                disabled={off}
                onChange={() => toggle(c.value)}
                className="mt-0.5"
              />
              <span className="min-w-0 flex-1 basis-40 text-sm">
                <span className="font-mono text-[13px] break-all text-chalk">{escapeName(c.value)}</span>
                {(c.chain.length > 0 || c.onlyOn) && (
                  <span className="ml-2 text-xs text-chalk-dim">
                    {c.chain.length > 0 && (
                      <>
                        {/* Said once to assistive technology; drawn with the names set in mono. */}
                        <span className="sr-only">also gives {c.chain.map(escapeName).join(", ")}</span>
                        <span aria-hidden="true" className="before:content-['also_gives_']">
                          {c.chain.map((r, i) => (
                            <span key={r}>
                              {i > 0 && ", "}
                              <span className="font-mono text-[12px] text-chalk">{escapeName(r)}</span>
                            </span>
                          ))}
                        </span>
                      </>
                    )}
                    {c.chain.length > 0 && c.onlyOn && " · "}
                    {c.onlyOn && `only on ${c.onlyOn.join(", ")}`}
                  </span>
                )}
                {blocked && <span className="block text-xs text-chalk-dim">{c.disabledReason}</span>}
              </span>
              {via !== undefined && via.length > 0 && (
                <span data-team-marker className="ml-auto inline-flex max-w-full items-baseline gap-1.5 pl-6 text-xs text-chalk sm:pl-0">
                  <Lock aria-hidden="true" size={12} strokeWidth={1.5} className="shrink-0 self-center text-chalk-dim" />
                  <span>
                    <span aria-hidden="true">✓ </span>
                    {own ? "also via" : "via"} <TeamsPhrase teams={via} />
                  </span>
                </span>
              )}
            </li>
          );
        })}
      </ul>
      {ordered.length === 0 && q !== "" && <p className="text-sm text-chalk-dim">No role matches “{escapeName(query.trim())}”.</p>}
    </div>
  );
}
