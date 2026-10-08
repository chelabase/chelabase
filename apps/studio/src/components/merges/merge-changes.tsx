import { useId, type ReactNode } from "react";
import {
  OBJ_KIND_LABEL,
  changeVerb,
  groupItems,
  itemName,
  itemSides,
  outcomeLabel,
  snapshotText,
  type Analysis,
  type Snapshot,
} from "@/lib/merges";

export interface MergeChangesProps {
  analysis: Analysis;
  sourceName: string;
  targetName: string;
}

const BADGE = "rounded-full border px-2 py-0.5 text-xs";
/** Coral: data loss and security are the hazards a merge can carry. */
export const HAZARD_BADGE = `${BADGE} border-destructive/60 text-destructive`;

export function Badge({ hazard = false, children }: { hazard?: boolean; children: ReactNode }) {
  return <span className={hazard ? HAZARD_BADGE : `${BADGE} text-muted-foreground`}>{children}</span>;
}

/** An object as the base, the target and the source have it, side by side as read-only text. */
export function ThreePanes({
  base,
  target,
  source,
  sourceName,
  targetName,
  subject,
}: {
  /** The object, for the text blocks' names. */
  subject: string;
  base: Snapshot | null;
  target: Snapshot | null;
  source: Snapshot | null;
  sourceName: string;
  targetName: string;
}) {
  const id = useId();
  const panes: [string, Snapshot | null][] = [
    ["Base (branch point)", base],
    [`${targetName} (target)`, target],
    [`${sourceName} (source)`, source],
  ];
  return (
    <div className="grid gap-2 md:grid-cols-3">
      {panes.map(([label, snap], i) => (
        <figure key={label} aria-labelledby={`${id}-${i}`} className="m-0 min-w-0 space-y-1">
          <figcaption id={`${id}-${i}`} className="text-xs text-muted-foreground">
            {label}
          </figcaption>
          {/* Focusable so a long definition can be scrolled from the keyboard. */}
          <pre
            tabIndex={0}
            aria-label={`${label} of ${subject}`}
            className="max-h-64 overflow-auto rounded-lg border bg-background p-2 font-mono text-xs whitespace-pre"
          >
            {snapshotText(snap)}
          </pre>
        </figure>
      ))}
    </div>
  );
}

type ReportedRow = {
  object?: { kind?: unknown; schema?: string | null; name?: string; parent?: string | null };
  change?: string;
  needed_by?: string[];
};

const kindText = (k: unknown) =>
  typeof k === "string" ? k.replaceAll("_", " ") : typeof k === "object" && k !== null ? String(Object.values(k)[0] ?? "object") : "object";

/** Every changed object, grouped by type, with its outcome; and what the merge doesn't move. */
export function MergeChanges({ analysis, sourceName, targetName }: MergeChangesProps) {
  const groups = groupItems(analysis.items);
  const reported = analysis.reported as ReportedRow[];
  return (
    <div className="space-y-4">
      {groups.length === 0 && <p className="text-sm text-muted-foreground">No schema changes on either side.</p>}
      {groups.map(([kind, items]) => (
        <div key={kind} className="space-y-1">
          <h3 className="text-sm font-medium">
            {OBJ_KIND_LABEL[kind]} ({items.length})
          </h3>
          <ul className="space-y-1">
            {items.map((item) => {
              const name = itemName(item.key);
              const sides = itemSides(item);
              return (
                <li key={item.id}>
                  <details aria-label={name} className="rounded-lg border px-3 py-2 text-sm">
                    <summary className="flex cursor-pointer flex-wrap items-center gap-2">
                      <span className="font-mono">{name}</span>
                      <Badge>{outcomeLabel(item.outcome)}</Badge>
                      {item.data_loss && <Badge hazard>Loses data</Badge>}
                      {item.security && <Badge hazard>Security</Badge>}
                      <span className="text-xs text-muted-foreground">
                        {sourceName}: {changeVerb(item.source)} · {targetName}: {changeVerb(item.target)}
                      </span>
                    </summary>
                    <div className="pt-2">
                      <ThreePanes {...sides} subject={name} sourceName={sourceName} targetName={targetName} />
                    </div>
                  </details>
                </li>
              );
            })}
          </ul>
        </div>
      ))}
      {reported.length > 0 && (
        <section aria-label="Not merged" className="space-y-1 text-sm">
          <h3 className="font-medium">Not merged</h3>
          <p className="text-xs text-muted-foreground">
            Merges move tables, columns, constraints, indexes, enums, sequences, policies, grants and roles. These
            other objects changed on {sourceName}, or merged objects use them: bring them over with your own steps.
          </p>
          <ul className="list-disc space-y-0.5 pl-5">
            {reported.map((r, i) => {
              const o = r.object ?? {};
              const qualified = [o.schema, o.parent, o.name].filter(Boolean).join(".");
              const how = r.change === "unchanged" ? `unchanged on ${sourceName}` : `${r.change ?? "changed"} on ${sourceName}`;
              return (
                <li key={i}>
                  {kindText(o.kind)} <span className="font-mono">{qualified}</span>: {how}
                  {r.needed_by && r.needed_by.length > 0 && (
                    <span className="text-muted-foreground"> (needed by {r.needed_by.join(", ")})</span>
                  )}
                </li>
              );
            })}
          </ul>
        </section>
      )}
    </div>
  );
}
