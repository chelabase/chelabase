import { useEffect, useMemo, useState } from "react";
import { useGetSqlSchema, type ChoiceBody, type Merge, type ResolutionBody } from "@chelabase/management-client";
import { CodeEditor } from "@/components/sql-editor/code-editor";
import { Button } from "@/components/ui/button";
import { CONFLICT_KIND_LABEL, itemName, type Analysis, type MergeConflict } from "@/lib/merges";
import type { PglsClient } from "@/lib/pgls/client";
import { sqlExtensions } from "@/lib/pgls/codemirror";
import type { SaveMerge } from "./merge-page";
import { ThreePanes } from "./merge-changes";

export interface MergeConflictsProps {
  databaseId: string;
  merge: Merge;
  analysis: Analysis;
  sourceName: string;
  targetName: string;
  /** Choices can change (a draft or rehearsed merge, nothing saving). */
  editable: boolean;
  save: SaveMerge;
  createClient?: () => Promise<PglsClient>;
}

type Pick = "keep_target" | "take_source" | "target_columns_first" | "own_sql";

const defaultCreateClient = () => import("@/lib/pgls/client").then((m) => m.createPglsClient());
const pickOf = (c: ChoiceBody): Pick => (typeof c === "string" ? c : "own_sql");

/**
 * The pgls worker and the target's schema, for completion in own SQL; started only once an own
 * SQL editor is open. The lifecycle is the SQL editor's.
 */
function useOwnSqlExtensions(
  enabled: boolean,
  databaseId: string,
  branchId: string,
  database: string,
  createClient: () => Promise<PglsClient>,
) {
  const [client, setClient] = useState<PglsClient | null>(null);
  useEffect(() => {
    if (!enabled) return;
    let disposed = false;
    let created: PglsClient | null = null;
    void (async () => {
      try {
        const c = await createClient();
        created = c;
        await c.ready;
        if (disposed) c.dispose();
        else setClient(c);
      } catch {
        if (created) created.dispose();
      }
    })();
    return () => {
      disposed = true;
      created?.dispose();
      setClient(null);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [enabled]);
  const schema = useGetSqlSchema(databaseId, branchId, { database }, { query: { enabled, staleTime: Infinity, retry: false } });
  const [ready, setReady] = useState(false);
  useEffect(() => {
    if (!client || schema.data === undefined) return;
    let stale = false;
    client.setSchema(JSON.stringify(schema.data)).then(
      () => !stale && setReady(true),
      () => !stale && setReady(false),
    );
    return () => {
      stale = true;
    };
  }, [client, schema.data]);
  return useMemo(() => sqlExtensions(ready && client ? client : null), [ready, client]);
}

/** Every conflict with base, target and source side by side, and a choice for each. */
export function MergeConflicts({
  databaseId,
  merge,
  analysis,
  sourceName,
  targetName,
  editable,
  save,
  createClient,
}: MergeConflictsProps) {
  const saved = new Map(merge.resolutions.map((r) => [r.conflict, r.choice]));
  const [picks, setPicks] = useState<Record<string, Pick | undefined>>(() =>
    Object.fromEntries(analysis.conflicts.map((c) => [c.id, saved.has(c.id) ? pickOf(saved.get(c.id)!) : undefined])),
  );
  const [ownSql, setOwnSql] = useState<Record<string, string>>(() =>
    Object.fromEntries(
      analysis.conflicts.map((c) => {
        const choice = saved.get(c.id);
        return [c.id, choice !== undefined && typeof choice !== "string" ? choice.own_sql : ""];
      }),
    ),
  );
  const [hint, setHint] = useState<string | null>(null);
  const anyOwnSql = Object.values(picks).includes("own_sql");
  const extensions = useOwnSqlExtensions(
    anyOwnSql,
    databaseId,
    merge.target_branch_id,
    merge.database,
    createClient ?? defaultCreateClient,
  );

  const names = new Map(analysis.items.map((i) => [i.id, itemName(i.key)]));
  const titleOf = (c: MergeConflict) => {
    const named = c.items.map((id) => names.get(id) ?? id);
    return named.length > 0 ? named.join(", ") : c.id;
  };

  function submit() {
    const resolutions: ResolutionBody[] = [];
    for (const c of analysis.conflicts) {
      const pick = picks[c.id];
      if (pick === undefined) continue;
      if (pick === "own_sql") {
        const text = (ownSql[c.id] ?? "").trim();
        if (text === "") {
          setHint(`Write the SQL for ${titleOf(c)}, or pick another choice.`);
          return;
        }
        resolutions.push({ conflict: c.id, choice: { own_sql: text } });
      } else {
        resolutions.push({ conflict: c.id, choice: pick });
      }
    }
    setHint(null);
    void save({ resolutions });
  }

  return (
    <div className="space-y-4">
      <p className="text-sm text-muted-foreground">
        Every conflict needs a choice before the rehearsal. Saving recomputes the plan; a rehearsed merge goes back to
        draft.
      </p>
      {analysis.conflicts.map((c) => {
        const title = titleOf(c);
        const options: [Pick, string][] =
          c.kind === "column_order"
            ? [
                ["target_columns_first", `${targetName}'s columns first, then ${sourceName}'s`],
                ["own_sql", "Write my own SQL"],
              ]
            : [
                ["keep_target", `Keep ${targetName}'s version`],
                ["take_source", `Take ${sourceName}'s version`],
                ["own_sql", "Write my own SQL"],
              ];
        return (
          <fieldset key={c.id} className="m-0 min-w-0 space-y-3 rounded-lg border p-3 text-sm">
            <legend className="px-1 font-medium">
              {CONFLICT_KIND_LABEL[c.kind] ?? c.kind}: <span className="font-mono">{title}</span>
            </legend>
            {c.kind === "column_order" ? (
              <p className="text-muted-foreground">
                Postgres can&apos;t reorder columns without rebuilding the table, so {sourceName}&apos;s order can&apos;t be
                taken as it is.
              </p>
            ) : (
              <ThreePanes subject={title} base={c.base} target={c.target} source={c.source} sourceName={sourceName} targetName={targetName} />
            )}
            <div className="flex flex-wrap gap-x-4 gap-y-1">
              {options.map(([value, label]) => (
                <label key={value} className="flex items-center gap-2">
                  <input
                    type="radio"
                    name={`choice-${c.id}`}
                    value={value}
                    checked={picks[c.id] === value}
                    disabled={!editable}
                    onChange={() => setPicks((p) => ({ ...p, [c.id]: value }))}
                  />
                  {label}
                </label>
              ))}
            </div>
            {picks[c.id] === "own_sql" && (
              <div className="space-y-1">
                <p className="text-xs text-muted-foreground">
                  Runs on {targetName} in place of this item. The rehearsal shows what it produced, for you to check.
                </p>
                <CodeEditor
                  label={`Own SQL for ${title}`}
                  initialDoc={ownSql[c.id] ?? ""}
                  extensions={extensions}
                  onChange={(text) => setOwnSql((s) => ({ ...s, [c.id]: text }))}
                  onRun={() => {}}
                />
              </div>
            )}
          </fieldset>
        );
      })}
      {hint && (
        <p role="alert" className="text-sm text-destructive">
          {hint}
        </p>
      )}
      <Button size="sm" variant="outline" disabled={!editable} onClick={submit}>
        Save choices
      </Button>
    </div>
  );
}
