import { useId, useState } from "react";
import { Search } from "lucide-react";
import { Checkbox } from "@/components/table-editor/panel-bits";
import type { RoleChoice } from "../role-picker";
import { escapeName } from "./escape-name";

export interface RoleChecklistProps {
  choices: readonly RoleChoice[];
  /** The roles of the member's own grant (never a locked one). */
  checked: readonly string[];
  /** The roles as last saved: a row whose box differs from it is marked changed. */
  baseline?: readonly string[];
  /** Roles given by a team (step 2): shown checked and disabled with their source; never part of `onChange`'s result. */
  locked?: readonly { value: string; via: string }[];
  onChange: (next: string[]) => void;
  disabled?: boolean;
}

/** Above this many roles a filter field appears. */
const FILTER_ABOVE = 8;

/**
 * A role list as checkboxes, checked roles first. Names are shown escaped (MH-38d) and passed on raw;
 * a role that brings others says "also gives …"; a role only an Admin may hold is disabled with its
 * reason; a team's role is checked, disabled and says where it comes from.
 */
export function RoleChecklist({ choices, checked, baseline, locked = [], onChange, disabled }: RoleChecklistProps) {
  const filterId = useId();
  const [query, setQueryState] = useState("");
  const lockedBy = new Map(locked.map((l) => [l.value, l.via]));
  const isOn = (v: string) => checked.includes(v) || lockedBy.has(v);
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
          const off = disabled || via !== undefined || blocked;
          return (
            <li
              key={c.value}
              data-changed={baseline !== undefined && via === undefined && baseline.includes(c.value) !== checked.includes(c.value) ? "true" : undefined}
              className="flex items-start gap-2 px-2 py-1.5 data-[changed=true]:shadow-[inset_2px_0_0_var(--sand)]"
            >
              <Checkbox
                aria-label={escapeName(c.value)}
                checked={isOn(c.value)}
                disabled={off}
                onChange={() => toggle(c.value)}
                className="mt-0.5"
              />
              <span className="min-w-0 text-sm">
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
                {via !== undefined && <span className="block text-xs text-chalk-dim">via {escapeName(via)}</span>}
                {blocked && <span className="block text-xs text-chalk-dim">{c.disabledReason}</span>}
              </span>
            </li>
          );
        })}
      </ul>
      {ordered.length === 0 && q !== "" && <p className="text-sm text-chalk-dim">No role matches “{escapeName(query.trim())}”.</p>}
    </div>
  );
}
