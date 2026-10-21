import { useEffect, useReducer, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { getGetTableQueryKey, useGetTable, type Grant, type Policy, type RoleRef, type Table } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import type { Mark, ProjectedColumn } from "@/lib/table-editor/changeset";
import { errorText } from "@/lib/errors";
import { useTableEditor } from "@/lib/table-editor/store";
import { cn } from "@/lib/utils";
import { AddConstraintButton, ConstraintActions } from "./constraints-section";
import { READ_ONLY_KIND, ROWS_QUERY } from "./rows-view";
import { TableActions } from "./table-actions";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";

export interface StructureViewProps {
  target: TableTarget;
  /** Why structure can't be changed here (protected branch, Viewer), or null. */
  reason?: string | null;
  /** The object's kind from the object list (`view`, …), for an object that isn't an editable table. */
  kind?: string;
}

const READ_ONLY = "Read-only here for now";
/** The last part of the query key a non-table object's definition is cached under. */
const OBJECT_DEFINITION = "object-definition";

const KIND_TEXT = { primary_key: "primary key", unique: "unique", foreign_key: "foreign key", check: "check", exclusion: "exclusion" } as const;

function Section({ title, note, children, action }: { title: string; note?: string; children: ReactNode; action?: ReactNode }) {
  return (
    <section className="space-y-2">
      <div className="flex items-baseline gap-3">
        <h3 className="text-sm font-semibold text-chalk">{title}</h3>
        {note && <span className="text-xs text-chalk-dim">{note}</span>}
        {action && <div className="ml-auto">{action}</div>}
      </div>
      {children}
    </section>
  );
}

function MarkTag({ mark }: { mark: Mark | null }) {
  if (!mark) return null;
  return (
    <span className={cn("ml-2 font-sans text-[11px]", mark === "dropped" ? "text-coral" : "text-sand")}>
      {mark === "added" ? "new" : mark}
    </span>
  );
}

const roleText = (r: RoleRef) => (r === "public" ? "PUBLIC" : r.role.name);

function Empty({ children }: { children: ReactNode }) {
  return <p className="text-[13px] text-chalk-dim">{children}</p>;
}

const rowClass = "border-b border-line px-2 py-1.5 align-top";

/** "identity always" / "identity by default" for a base identity column (its Default cell), else null. */
function identityText(base: Table, c: ProjectedColumn): string | null {
  if (c.original === null) return null;
  const generation = base.columns.find((b) => b.name === c.original)?.identity?.generation;
  return generation === "always" ? "identity always" : generation === "by_default" ? "identity by default" : null;
}

/**
 * The Structure tab: Columns (a row opens its form), Constraints, the read-only Indexes, Policies and
 * Grants, and the Table's own settings. Pending changes show with their marks.
 */
export function StructureView(props: StructureViewProps) {
  return props.target.notTable ? <ObjectStructure target={props.target} kind={props.kind} /> : <TableStructureView {...props} />;
}

/**
 * The Structure tab of an object that isn't an editable table (a view, materialized view, foreign or partitioned
 * table): its columns with their SQL types, read-only, from the table endpoint (asked here only, never on the Rows
 * tab). When the endpoint can't answer, the columns are the names its rows came back with on the Rows tab.
 */
function ObjectStructure({ target, kind }: { target: TableTarget; kind?: string }) {
  const { databaseId, branchId, database, schema, table } = target;
  const params = database ? { database } : undefined;
  // Its own cache entry (under the table's key, so a refresh of the table's queries reaches it): the Rows tab's
  // disabled table queries must never pick a view's definition up from the cache. Not retried: a failure falls back
  // to the names. Fresh for the app's default 10s, set here so leaving the tab and coming back doesn't ask again.
  const got = useGetTable(databaseId, branchId, schema, table, params, {
    query: { retry: false, staleTime: 10_000, queryKey: [...getGetTableQueryKey(databaseId, branchId, schema, table, params), OBJECT_DEFINITION] },
  });
  const note = READ_ONLY_KIND[kind ?? ""] ?? "This object is read-only here.";
  return (
    <div className="min-h-0 flex-1 space-y-8 overflow-auto p-4">
      {got.data ? (
        <>
          <p className="text-[13px] text-chalk-dim">{note}</p>
          <Section title="Columns">
            {got.data.columns.length === 0 ? (
              <Empty>No columns</Empty>
            ) : (
              <table className="w-full border-collapse text-left text-[13px]">
                <thead className="text-xs text-chalk-dim">
                  <tr>
                    <th className={cn(rowClass, "font-normal")}>Name</th>
                    <th className={cn(rowClass, "font-normal")}>Type</th>
                    <th className={cn(rowClass, "font-normal")}>Null</th>
                  </tr>
                </thead>
                <tbody>
                  {got.data.columns.map((c) => (
                    <tr key={c.name} data-column={c.name}>
                      <td className={cn(rowClass, "font-mono text-chalk")}>{c.name}</td>
                      <td data-testid="column-type" className={cn(rowClass, "font-mono text-chalk-dim")}>
                        {c.data_type}
                      </td>
                      <td className={cn(rowClass, "text-chalk-dim")}>{c.not_null ? "not null" : "null"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </Section>
        </>
      ) : got.error ? (
        <ColumnNames target={target} note={note} />
      ) : (
        <p className="text-[13px] text-chalk-dim">Loading the columns…</p>
      )}
    </div>
  );
}

/** The fallback when the definition can't be read: the column names the object's rows came back with. */
function ColumnNames({ target, note }: { target: TableTarget; note: string }) {
  const queryClient = useQueryClient();
  // Read again whenever the query cache changes (the rows may arrive after this tab opened).
  const [, cacheChanged] = useReducer((n: number) => n + 1, 0);
  useEffect(() => queryClient.getQueryCache().subscribe(cacheChanged), [queryClient]);
  const { databaseId, branchId, database, role, schema, table } = target;
  const pages = queryClient.getQueriesData<{ rows?: Record<string, unknown>[] }>({ queryKey: [ROWS_QUERY, databaseId, branchId, database, role, `${schema}.${table}`] });
  const first = pages.map(([, page]) => page?.rows?.[0]).find((r) => r !== undefined);
  const names = first ? Object.keys(first) : [];
  return (
    <>
      <p className="text-[13px] text-chalk-dim">
        <span>{note}</span> <span>Column types aren&apos;t available for this object.</span>
      </p>
      <Section title="Columns">
        {names.length === 0 ? (
          <Empty>No rows read yet, so no columns to show.</Empty>
        ) : (
          <ul className="space-y-1 font-mono text-[13px] text-chalk">
            {names.map((n) => (
              <li key={n}>{n}</li>
            ))}
          </ul>
        )}
      </Section>
    </>
  );
}

function TableStructureView({ target, reason: structureReason = null }: StructureViewProps) {
  const { base, projected, error, reason: dropped } = useTableStructure(target);
  // A staged drop of the table leaves only the changes panel to work with.
  const reason = structureReason ?? dropped;
  const openPanel = useTableEditor((s) => s.openPanel);
  if (error) {
    return (
      <p role="alert" className="p-6 text-sm text-coral">
        {errorText(error)}
      </p>
    );
  }
  if (!base || !projected) {
    return <p className="p-6 text-sm text-chalk-dim">Loading the table…</p>;
  }
  const disabled = reason !== null;
  const open = (c: ProjectedColumn) => (c.mark === "dropped" ? openPanel("changes") : openPanel("column", { column: c.name }));
  return (
    <div className="min-h-0 flex-1 space-y-8 overflow-auto p-4">
      {dropped && !structureReason && <p className="text-[13px] text-chalk-dim">{dropped}</p>}
      <Section
        title="Columns"
        action={
          <Button size="sm" variant="outline" disabled={disabled} title={reason ?? undefined} onClick={() => openPanel("column", { column: null })}>
            <Plus aria-hidden="true" strokeWidth={1.5} />
            Add column
          </Button>
        }
      >
        <table className="w-full border-collapse text-left text-[13px]">
          <thead className="text-xs text-chalk-dim">
            <tr>
              <th className={cn(rowClass, "font-normal")}>Name</th>
              <th className={cn(rowClass, "font-normal")}>Type</th>
              <th className={cn(rowClass, "font-normal")}>Null</th>
              <th className={cn(rowClass, "font-normal")}>Default</th>
              <th className={cn(rowClass, "font-normal")}>Comment</th>
            </tr>
          </thead>
          <tbody>
            {projected.columns.map((c) => (
              <tr key={`${c.original ?? "+"}:${c.name}`} className={cn(c.mark === "dropped" && "text-chalk-dim line-through decoration-coral")}>
                <td className={cn(rowClass, "font-mono")}>
                  <button
                    type="button"
                    disabled={disabled}
                    onClick={() => open(c)}
                    className="text-left text-chalk outline-none hover:underline focus-visible:outline-2 focus-visible:outline-sand disabled:hover:no-underline"
                  >
                    {c.name}
                  </button>
                  <MarkTag mark={c.mark} />
                </td>
                <td className={cn(rowClass, "font-mono text-chalk-dim")}>{c.data_type}</td>
                <td className={cn(rowClass, "text-chalk-dim")}>{c.not_null ? "not null" : "null"}</td>
                <td className={cn(rowClass, "font-mono text-chalk-dim")}>{c.default ?? identityText(base, c) ?? ""}</td>
                <td className={cn(rowClass, "text-chalk-dim")}>{c.comment ?? ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </Section>

      <Section title="Constraints" action={<AddConstraintButton target={target} reason={reason} />}>
        {projected.constraints.length === 0 ? (
          <Empty>No constraints</Empty>
        ) : (
          <ul className="space-y-1 text-[13px]">
            {projected.constraints.map((k) => (
              <li key={`${k.kind}:${k.name}`} className={cn("flex flex-wrap items-baseline gap-x-3 border-b border-line py-1.5", k.mark === "dropped" && "line-through decoration-coral")}>
                <span className="font-mono text-chalk">{k.name}</span>
                <span className="text-xs text-chalk-dim">{KIND_TEXT[k.kind]}</span>
                <span className="font-mono text-xs text-chalk-dim">
                  {k.definition ??
                    `(${k.columns.join(", ")})${k.references ? ` → ${k.references.schema}.${k.references.table}(${k.references.columns.join(", ")})` : ""}`}
                </span>
                {!k.validated && <span className="text-xs text-chalk-dim">not valid</span>}
                <MarkTag mark={k.mark} />
                <ConstraintActions target={target} constraint={k} reason={reason} />
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title="Indexes" note={READ_ONLY}>
        {base.indexes.length === 0 ? (
          <Empty>No other indexes</Empty>
        ) : (
          <ul className="space-y-1 text-[13px]">
            {base.indexes.map((ix) => (
              <li key={ix.name} className="flex flex-wrap items-baseline gap-x-3 border-b border-line py-1.5">
                <span className="font-mono text-chalk">{ix.name}</span>
                <span className="font-mono text-xs text-chalk-dim">
                  {ix.unique ? "unique " : ""}
                  {ix.method} ({ix.keys.join(", ")})
                  {ix.include.length > 0 ? ` include (${ix.include.join(", ")})` : ""}
                  {ix.predicate ? ` where ${ix.predicate}` : ""}
                </span>
                {!ix.valid && <span className="text-xs text-coral">invalid</span>}
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title="Policies" note={READ_ONLY}>
        {base.policies.length === 0 ? (
          <Empty>No policies</Empty>
        ) : (
          <ul className="space-y-1 text-[13px]">
            {base.policies.map((p: Policy) => (
              <li key={p.name} className="space-y-0.5 border-b border-line py-1.5">
                <div className="flex flex-wrap items-baseline gap-x-3">
                  <span className="font-mono text-chalk">{p.name}</span>
                  <span className="text-xs text-chalk-dim">
                    {p.permissive ? "permissive" : "restrictive"} · {String(p.command).toUpperCase()} · {p.roles.map(roleText).join(", ")}
                  </span>
                </div>
                {p.using && <p className="font-mono text-xs text-chalk-dim">using ({p.using})</p>}
                {p.with_check && <p className="font-mono text-xs text-chalk-dim">with check ({p.with_check})</p>}
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title="Grants" note={READ_ONLY}>
        {base.grants.length === 0 ? (
          <Empty>No grants</Empty>
        ) : (
          <ul className="space-y-1 text-[13px]">
            {grantRows(base.grants).map(([grantee, privileges]) => (
              <li key={grantee} className="flex flex-wrap items-baseline gap-x-3 border-b border-line py-1.5">
                <span className="font-mono text-chalk">{grantee}</span>
                <span className="text-xs text-chalk-dim">{privileges.join(", ")}</span>
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title="Table">
        <dl className="grid grid-cols-[8rem_1fr] gap-x-4 gap-y-1.5 text-[13px]">
          <dt className="text-chalk-dim">Name</dt>
          <dd className="font-mono text-chalk">
            {projected.schema}.{projected.name}
          </dd>
          <dt className="text-chalk-dim">Owner</dt>
          <dd className="font-mono text-chalk">{base.owner.name}</dd>
          <dt className="text-chalk-dim">Comment</dt>
          <dd className="text-chalk">{projected.comment ?? <span className="text-chalk-dim">none</span>}</dd>
          <dt className="text-chalk-dim">Row-level security</dt>
          <dd className="text-chalk">
            {projected.rls_enabled ? "on" : "off"}
            {projected.rls_enabled !== base.rls_enabled && <span className="ml-2 text-[11px] text-sand">pending</span>}
          </dd>
        </dl>
        <TableActions target={target} reason={reason} />
      </Section>
    </div>
  );
}

/** Grants grouped by grantee: `billing_admin → SELECT, UPDATE`. */
function grantRows(grants: Grant[]): [string, string[]][] {
  const by = new Map<string, string[]>();
  for (const g of grants) {
    const who = roleText(g.grantee);
    by.set(who, [...(by.get(who) ?? []), `${g.privilege}${g.grantable ? " (grantable)" : ""}`]);
  }
  return [...by.entries()];
}
