import { useId, useState } from "react";
import { useListTeams } from "@chelabase/management-client";
import { Checkbox } from "@/components/table-editor/panel-bits";
import { errorText } from "@/lib/errors";
import { escapeName } from "./hub/escape-name";

export interface TeamPickerProps {
  /** The chosen team ids. */
  value: readonly string[];
  onChange: (next: string[]) => void;
  disabled?: boolean;
}

/** Above this many teams a filter field appears. */
const FILTER_ABOVE = 8;

/** The next selection after toggling `id`: added at the end, removed in place; never twice. */
export function toggleTeam(value: readonly string[], id: string): string[] {
  return value.includes(id) ? value.filter((v) => v !== id) : [...value, id];
}

/**
 * A filterable checklist of the tenant's teams (the invite's and the approval's "Teams"). Renders
 * nothing when the tenant has no team; a failed read says so quietly and the dialog still works.
 */
export function TeamPicker({ value, onChange, disabled }: TeamPickerProps) {
  const teams = useListTeams();
  const filterId = useId();
  const [query, setQuery] = useState("");
  if (teams.isPending) return null;
  if (teams.isError) return <p className="text-sm text-chalk-dim">Couldn't load teams: {errorText(teams.error)}</p>;
  const all = teams.data;
  if (all.length === 0) return null;
  const q = query.trim().toLowerCase();
  const shown = all.filter((t) => q === "" || t.name.toLowerCase().includes(q));
  return (
    <fieldset className="m-0 min-w-0 space-y-2 border-0 p-0 pb-4" disabled={disabled}>
      <legend className="mb-1 text-sm">
        <span className="font-medium">Teams</span>
        <span className="font-normal text-chalk-dim"> · their access applies as long as they are in the team</span>
      </legend>
      {all.length > FILTER_ABOVE && (
        <div>
          <label htmlFor={filterId} className="sr-only">
            {`Filter ${all.length} teams`}
          </label>
          <input
            id={filterId}
            type="search"
            value={query}
            placeholder={`Filter ${all.length} teams`}
            onChange={(e) => setQuery(e.target.value)}
            className="h-8 w-full border border-input bg-ink px-2 text-sm text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
          />
        </div>
      )}
      <ul className="divide-y divide-line border border-line">
        {shown.map((t) => (
          <li key={t.id} className="flex items-start gap-2 px-2 py-1.5">
            <Checkbox aria-label={`Team ${t.name}`} checked={value.includes(t.id)} onChange={() => onChange(toggleTeam(value, t.id))} className="mt-0.5" />
            <span className="min-w-0 text-sm">
              <span className="text-chalk">{escapeName(t.name)}</span>
              <span className="ml-2 text-xs text-chalk-dim">
                {t.member_count} {t.member_count === 1 ? "member" : "members"}
              </span>
            </span>
          </li>
        ))}
      </ul>
      {shown.length === 0 && <p className="text-sm text-chalk-dim">No team matches “{escapeName(query.trim())}”.</p>}
    </fieldset>
  );
}
