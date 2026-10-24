import { Select } from "@/components/ui/select";
import { Fragment, useId, useState, type ReactNode } from "react";
import { ChevronDown, ChevronRight, Lock } from "lucide-react";
import type { DefaultObject, Grant, ObjectPrivileges, SchemaPrivileges } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { escapeName } from "../hub/escape-name";
import { plural } from "../member-page/logins";
import { RoleSwitch, stagedCell, Tag, TriCheckbox } from "./bits";
import {
  COLUMN_PRIVILEGES,
  FUNCTION_KINDS,
  holdsOption,
  KIND_WORD,
  otherGrantors,
  RELATION_KINDS,
  RLS_KINDS,
  type Edit,
  type EditTarget,
} from "./model";
import type { PrivilegeEditorState } from "./use-privilege-editor";

const upper = (p: string) => p.toUpperCase();
const ownedBy = (owner: string) => `owned by ${escapeName(owner)}: change in SQL as ${escapeName(owner)}`;

interface Ctx {
  editor: PrivilegeEditorState;
  /** The caller may change privileges here (an Admin, R6), and no review is open. */
  editable: boolean;
  /** Admin: the grant-option and every-RLS-table actions show at all. */
  admin: boolean;
}

/** The value a cell shows: its staged value, else what the read says. */
function desired(editor: PrivilegeEditorState, e: Edit): boolean {
  const s = editor.staged(e);
  if (s?.kind === "privilege") return s.grant;
  if (s?.kind === "all_rows") return s.enabled;
  if (s?.kind === "option") return true;
  if (e.kind === "privilege") return e.grant;
  return e.kind === "all_rows" ? e.enabled : false;
}

/** A privilege cell's edit, with what it holds now as its `grant`. */
type PrivilegeEdit = Extract<Edit, { kind: "privilege" }>;
const cell = (target: EditTarget, privilege: string, held: boolean, column: string | null = null): PrivilegeEdit => ({ kind: "privilege", target, privilege, column, grant: held });
const flip = (e: PrivilegeEdit, value: boolean): Edit => ({ ...e, grant: value });

/** The tri-state of a group of cells, over the ones that can change (all of them when none can). */
function groupState(editor: PrivilegeEditorState, cells: { edit: Edit; settable: boolean }[]): boolean | "mixed" {
  const pool = cells.some((c) => c.settable) ? cells.filter((c) => c.settable) : cells;
  if (pool.length === 0) return false;
  const on = pool.filter((c) => desired(editor, c.edit)).length;
  return on === 0 ? false : on === pool.length ? true : "mixed";
}

/** Toggling a group: all on unless every settable cell is on already. */
function toggleGroup(editor: PrivilegeEditorState, cells: { edit: PrivilegeEdit; settable: boolean }[], state: boolean | "mixed") {
  const value = state !== true;
  editor.stage(cells.filter((c) => c.settable && desired(editor, c.edit) !== value).map((c) => flip(c.edit, value)));
}

/** The privileges editor's body: one block per schema, the first open (H5). */
export function PrivilegeEditor({ editor, editable, admin, role, dbOwner }: Ctx & { role: string; dbOwner: string | null }) {
  const { head } = editor;
  if (head.isError) {
    return (
      <p role="alert" className="text-sm text-coral">
        Couldn't read the privileges: {head.error instanceof Error ? head.error.message : "unknown error"}
      </p>
    );
  }
  if (!head.data) return <p className="text-sm text-chalk-dim">Reading privileges…</p>;
  if (head.data.schemas.length === 0) return <p className="text-sm text-chalk-dim">This database has no schemas to grant on.</p>;
  return (
    <div className="space-y-4">
      {editor.applied && (
        <p role="status" className="flex items-center gap-2 text-[13px] text-seafoam">
          <span aria-hidden="true" className="size-2 bg-seafoam" />
          {editor.applied}
        </p>
      )}
      {head.data.schemas.map((s) => (
        <SchemaBlock key={s.name} schema={s} editor={editor} editable={editable} admin={admin} role={role} dbOwner={dbOwner} />
      ))}
    </div>
  );
}

function countsText(objects: ObjectPrivileges[], more: boolean) {
  const plus = more ? "+" : "";
  const rel = objects.filter((o) => RELATION_KINDS.has(o.kind)).length;
  const seq = objects.filter((o) => o.kind === "sequence").length;
  const fn = objects.filter((o) => FUNCTION_KINDS.has(o.kind)).length;
  const parts = [`${rel}${plus} ${rel === 1 ? "table or view" : "tables and views"}`];
  if (seq > 0) parts.push(`${seq}${plus} ${seq === 1 ? "sequence" : "sequences"}`);
  if (fn > 0) parts.push(`${fn}${plus} ${fn === 1 ? "function" : "functions"}`);
  return parts.join(" · ");
}

function SchemaBlock({ schema, editor, editable, admin, role, dbOwner }: Ctx & { schema: SchemaPrivileges; role: string; dbOwner: string | null }) {
  const open = editor.isOpen(schema.name);
  const read = editor.schemaRead(schema.name);
  const objects = read?.objects ?? [];
  const contentId = useId();
  const held = [...new Set(schema.grants.map((g) => g.privilege))];
  const detail = read && !read.loading && !read.error ? `schema · ${countsText(objects, read.more)}` : held.length > 0 ? `schema · ${held.join(", ")}` : "schema";
  return (
    <section className="border border-line">
      <button
        type="button"
        aria-expanded={open}
        aria-controls={contentId}
        onClick={() => editor.toggleSchema(schema.name)}
        className={cn("flex w-full items-center gap-2.5 bg-deep px-3.5 py-2.5 text-left outline-none hover:bg-chalk/[0.06] focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand", open && "border-b border-line")}
      >
        {open ? <ChevronDown aria-hidden="true" size={16} strokeWidth={1.5} /> : <ChevronRight aria-hidden="true" size={16} strokeWidth={1.5} />}
        <span className="font-mono text-sm font-medium text-chalk">{escapeName(schema.name)}</span>{" "}
        <span className="text-[13px] text-chalk-dim">{detail}</span>
      </button>
      {open && (
        <div id={contentId} className="divide-y divide-line">
          <SchemaPrivilegesBlock schema={schema} editor={editor} editable={editable} admin={admin} />
          {read?.error ? (
            <p role="alert" className="p-3.5 text-sm text-coral">
              Couldn't read the objects in {escapeName(schema.name)}: {read.error instanceof Error ? read.error.message : "unknown error"}
            </p>
          ) : !read || (read.loading && objects.length === 0) ? (
            <p className="p-3.5 text-sm text-chalk-dim">Reading the objects in {escapeName(schema.name)}…</p>
          ) : (
            <>
              <Matrix schema={schema.name} objects={objects.filter((o) => RELATION_KINDS.has(o.kind))} editor={editor} editable={editable} admin={admin} />
              <AllRows schema={schema.name} objects={objects.filter((o) => RLS_KINDS.has(o.kind) && o.rls_enabled)} editor={editor} editable={editable} admin={admin} role={role} />
              <Sequences objects={objects.filter((o) => o.kind === "sequence")} editor={editor} editable={editable} admin={admin} />
              <Functions objects={objects.filter((o) => FUNCTION_KINDS.has(o.kind))} editor={editor} editable={editable} admin={admin} />
              {read.more && (
                <div className="p-3.5">
                  <Button variant="outline" size="sm" disabled={read.loading} onClick={() => editor.loadMore(schema.name)}>
                    {read.loading ? "Loading more…" : `Load more objects in ${escapeName(schema.name)}`}
                  </Button>
                </div>
              )}
              <FutureObjects schema={schema} objects={objects} editor={editor} editable={editable} admin={admin} dbOwner={dbOwner} />
            </>
          )}
        </div>
      )}
    </section>
  );
}

function Block({ title, description, children, flush, frame }: { title: string; description?: string; children: ReactNode; flush?: boolean; frame?: boolean }) {
  return (
    <div className={flush ? "pt-3.5" : frame ? "m-3.5 border border-line p-3.5" : "p-3.5"}>
      <div className={cn("mb-2.5 flex flex-wrap items-baseline gap-x-2.5", flush && "px-3.5")}>
        <h3 className="text-sm font-semibold">{title}</h3>
        {description && <span className="text-[13px] text-chalk-dim">{description}</span>}
      </div>
      {children}
    </div>
  );
}

/** Notes under or beside an object: grant options (revoke only, MH-29), other grantors (P30), owner. */
function Notes({ editor, target, grants, privileges, owner, settable, admin, editable, extra }: {
  editor: PrivilegeEditorState;
  target: Exclude<EditTarget, { kind: "default" }>;
  grants: readonly Grant[];
  privileges: readonly string[];
  owner: string;
  settable: boolean;
  admin: boolean;
  editable: boolean;
  extra?: ReactNode;
}) {
  const notes: ReactNode[] = [];
  if (!settable) notes.push(<span key="owner">{ownedBy(owner)}</span>);
  for (const p of privileges) {
    if (!holdsOption(grants, p)) continue;
    const option: Edit = { kind: "option", target, privilege: p };
    const revoking = editor.staged(option) !== undefined;
    const privilegeRevoked = !desired(editor, cell(target, p, true));
    if (privilegeRevoked) continue;
    notes.push(
      revoking ? (
        <span key={`option-${p}`} className="text-sand">
          {p} grant option: revoke staged{admin && editable && settable ? " · " : ""}
          {admin && editable && settable && (
            <button type="button" onClick={() => editor.unstage(option)} className="underline underline-offset-4 outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand">
              Keep grant option
            </button>
          )}
        </span>
      ) : (
        <span key={`option-${p}`}>
          {p} with grant option{admin && settable ? " · " : ""}
          {admin && settable && (
            <button
              type="button"
              disabled={!editable}
              onClick={() => editor.stage([option])}
              className="text-chalk underline underline-offset-4 outline-none hover:text-sand focus-visible:outline-2 focus-visible:outline-sand disabled:opacity-50"
            >
              Revoke grant option
            </button>
          )}
        </span>
      ),
    );
  }
  for (const p of privileges) {
    const by = otherGrantors(grants, p, owner);
    if (by.length > 0) notes.push(<span key={`by-${p}`}>{`${p} granted by ${by.map(escapeName).join(", ")}`}</span>);
  }
  if (extra) notes.push(extra);
  if (notes.length === 0) return null;
  return <span className="flex flex-col gap-0.5 text-xs text-chalk-dim">{notes}</span>;
}

function SchemaPrivilegesBlock({ schema, editor, editable, admin }: Ctx & { schema: SchemaPrivileges }) {
  const offered = (editor.head.data?.offered.schema ?? []).map(upper);
  const target: EditTarget = { kind: "schema", schema: schema.name };
  const settable = schema.owner_settable;
  return (
    <Block title="Schema" description={`Without USAGE, nothing inside ${escapeName(schema.name)} is reachable.`}>
      <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
        {offered.map((p) => (
          <PrivilegeBox key={p} editor={editor} edit={cell(target, p, schema.grants.some((g) => g.privilege === p))} label={`${p} on schema ${escapeName(schema.name)}`} text={p} disabled={!editable || !settable} />
        ))}
        <Notes editor={editor} target={target} grants={schema.grants} privileges={offered} owner={schema.owner} settable={settable} admin={admin} editable={editable} />
      </div>
    </Block>
  );
}

/** One labelled privilege checkbox (schema, sequence, function, future objects rows). */
function PrivilegeBox({ editor, edit, label, text, disabled }: { editor: PrivilegeEditorState; edit: PrivilegeEdit; label: string; text: string; disabled: boolean }) {
  const value = desired(editor, edit);
  const isStaged = editor.staged(edit) !== undefined;
  return (
    <label className={cn("inline-flex h-8 items-center gap-2 px-1.5 font-mono text-xs", isStaged && stagedCell, disabled ? "text-chalk-dim" : "text-chalk")}>
      <TriCheckbox label={label} checked={value} disabled={disabled} onChange={() => editor.stage([flip(edit, !value)])} />
      {text}
    </label>
  );
}

function Matrix({ schema, objects, editor, editable, admin }: Ctx & { schema: string; objects: ObjectPrivileges[] }) {
  const offered = (editor.head.data?.offered.table ?? []).map(upper);
  const [drilled, setDrilled] = useState<ReadonlySet<string>>(new Set());
  if (objects.length === 0) {
    return (
      <Block title="Tables and views">
        <p className="text-sm text-chalk-dim">No tables or views in {escapeName(schema)}.</p>
      </Block>
    );
  }
  const rowCells = (o: ObjectPrivileges) => {
    const target: EditTarget = { kind: "relation", schema: o.schema, name: o.name };
    return offered.map((p) => ({ edit: cell(target, p, o.grants.some((g) => g.privilege === p)), settable: o.owner_settable }));
  };
  const th = "border-r border-b border-line-strong bg-channel px-1 py-1.5 font-mono text-[11px] font-normal text-chalk-dim";
  const td = "h-9 border-r border-b border-line px-1 text-center";
  return (
    <Block title="Tables and views" description="G marks a privilege held with grant option." flush>
      <div className="relative overflow-x-auto">
        <table aria-label={`Tables and views in ${schema}`} className="w-full min-w-[52rem] border-collapse text-sm">
          <thead>
            <tr>
              <th scope="col" className={cn(th, "pl-2.5 text-left font-sans text-xs")}>
                Object
              </th>
              {offered.map((p) => (
                <th key={p} scope="col" className={th}>
                  {p}
                </th>
              ))}
              <th scope="col" className={cn(th, "font-sans text-xs")}>
                All
              </th>
              <th scope="col" className={cn(th, "border-r-0 pl-2.5 text-left font-sans text-xs")}>
                Notes
              </th>
            </tr>
            <tr>
              <td className={cn(td, "bg-channel pl-2.5 text-left text-xs text-chalk-dim")}>Every table</td>
              {offered.map((p, i) => {
                const cells = objects.map((o) => rowCells(o)[i]);
                const state = groupState(editor, cells);
                return (
                  <td key={p} className={cn(td, "bg-channel")}>
                    <TriCheckbox label={`${p} on every table in ${schema}`} checked={state} disabled={!editable || !cells.some((c) => c.settable)} onChange={() => toggleGroup(editor, cells, state)} />
                  </td>
                );
              })}
              <td className={cn(td, "bg-channel")} />
              <td className={cn(td, "border-r-0 bg-channel")} />
            </tr>
          </thead>
          <tbody>
            {objects.map((o) => {
              const cells = rowCells(o);
              const all = groupState(editor, cells);
              const settable = o.owner_settable;
              const isOpen = drilled.has(o.name);
              const columnGrants = o.columns.reduce((n, c) => n + c.grants.length, 0);
              const kind = KIND_WORD[o.kind];
              return (
                <Fragment key={o.name}>
                  <tr className={cn("even:bg-chalk/[0.045]", !settable && "text-chalk-dim")}>
                    <td className={cn(td, "pl-2.5 text-left")}>
                      <span className="flex items-center gap-2">
                        {settable ? (
                          <button
                            type="button"
                            aria-label={`Columns of ${escapeName(o.name)}`}
                            aria-expanded={isOpen}
                            onClick={() => setDrilled((d) => (d.has(o.name) ? new Set([...d].filter((n) => n !== o.name)) : new Set([...d, o.name])))}
                            className="flex size-6 shrink-0 items-center justify-center text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand"
                          >
                            {isOpen ? <ChevronDown aria-hidden="true" size={14} strokeWidth={1.5} /> : <ChevronRight aria-hidden="true" size={14} strokeWidth={1.5} />}
                          </button>
                        ) : (
                          <span className="flex size-6 shrink-0 items-center justify-center" title={ownedBy(o.owner)}>
                            <Lock aria-hidden="true" size={14} strokeWidth={1.5} />
                          </span>
                        )}
                        <span className={cn("font-mono text-xs", settable ? "text-chalk" : "text-chalk-dim")}>{escapeName(o.name)}</span>{" "}
                        {kind && <Tag>{kind}</Tag>}
                      </span>
                    </td>
                    {cells.map(({ edit }) => {
                      const value = desired(editor, edit);
                      const staged = editor.staged(edit) !== undefined;
                      const g = value && holdsOption(o.grants, edit.privilege) && !editor.staged({ kind: "option", target: edit.target as Exclude<EditTarget, { kind: "default" }>, privilege: edit.privilege });
                      return (
                        <td key={edit.privilege} className={cn(td, staged && stagedCell)}>
                          <span className="relative inline-flex">
                            <TriCheckbox label={`${edit.privilege} on ${escapeName(o.name)}`} checked={value} disabled={!editable || !settable} onChange={() => editor.stage([flip(edit, !value)])} />
                            {g && (
                              <span aria-hidden="true" className="absolute -top-1.5 -right-2 text-[9px] leading-none font-semibold text-chalk-dim">
                                G
                              </span>
                            )}
                          </span>
                        </td>
                      );
                    })}
                    <td className={td}>
                      <TriCheckbox label={`All privileges on ${escapeName(o.name)}`} checked={all} disabled={!editable || !settable} onChange={() => toggleGroup(editor, cells, all)} />
                    </td>
                    <td className={cn(td, "min-w-56 border-r-0 py-1 pl-2.5 text-left")}>
                      <Notes
                        editor={editor}
                        target={{ kind: "relation", schema: o.schema, name: o.name }}
                        grants={o.grants}
                        privileges={offered}
                        owner={o.owner}
                        settable={settable}
                        admin={admin}
                        editable={editable}
                        extra={columnGrants > 0 ? <span key="columns">{plural(columnGrants, "column grant")}</span> : undefined}
                      />
                    </td>
                  </tr>
                  {isOpen && (
                    <tr>
                      <td colSpan={offered.length + 3} className="border-b border-line bg-deep py-2.5 pr-2.5 pl-9 text-left">
                        <Columns object={o} offered={offered} editor={editor} editable={editable} />
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
          </tbody>
        </table>
      </div>
    </Block>
  );
}

/** A table's column grid: a table-wide privilege shows checked and fixed on every column. */
function Columns({ object, offered, editor, editable }: { object: ObjectPrivileges; offered: string[]; editor: PrivilegeEditorState; editable: boolean }) {
  const target: EditTarget = { kind: "relation", schema: object.schema, name: object.name };
  const privileges = COLUMN_PRIVILEGES.filter((p) => offered.includes(p));
  const cellClass = "flex h-[30px] items-center justify-center border-r border-b border-line px-2 font-mono text-[11px]";
  return (
    <div className="space-y-2">
      <p className="text-xs text-chalk-dim">{`Column privileges on ${escapeName(object.name)}. A table-wide privilege covers every column.`}</p>
      <div className="relative overflow-x-auto">
        <div className="grid w-max border border-line" style={{ gridTemplateColumns: `minmax(10rem, auto) repeat(${privileges.length}, 4.5rem)` }}>
          <span className={cn(cellClass, "justify-start bg-channel pl-2.5 text-chalk-dim")}>column</span>
          {privileges.map((p) => (
            <span key={p} className={cn(cellClass, "bg-channel text-chalk-dim")}>
              {p}
            </span>
          ))}
          {object.columns.map((c) => (
            <Fragment key={c.name}>
              <span className={cn(cellClass, "justify-start pl-2.5 text-xs text-chalk")}>{escapeName(c.name)}</span>
              {privileges.map((p) => {
                const tableWide = desired(editor, cell(target, p, object.grants.some((g) => g.privilege === p)));
                const edit = cell(target, p, c.grants.some((g) => g.privilege === p), c.name);
                const value = tableWide || desired(editor, edit);
                const staged = !tableWide && editor.staged(edit) !== undefined;
                return (
                  <span key={p} className={cn(cellClass, staged && stagedCell)}>
                    <TriCheckbox label={`${p} on ${escapeName(object.name)}.${escapeName(c.name)}`} checked={value} disabled={!editable || tableWide} onChange={() => editor.stage([flip(edit, !value)])} />
                  </span>
                );
              })}
            </Fragment>
          ))}
        </div>
      </div>
    </div>
  );
}

function AllRows({ schema, objects, editor, editable, admin, role }: Ctx & { schema: string; objects: ObjectPrivileges[]; role: string }) {
  if (objects.length === 0) return null;
  const rows = objects.map((o) => {
    const edit: Edit = { kind: "all_rows", schema: o.schema, name: o.name, enabled: o.all_rows != null };
    return { o, edit, on: desired(editor, edit), staged: editor.staged(edit) !== undefined };
  });
  const off = rows.filter((r) => !r.on && r.o.owner_settable);
  return (
    <Block frame title="All rows" description={`Tables with row-level security on. On, ${escapeName(role)} sees and changes every row through a policy Chelabase adds.`}>
      <ul className="divide-y divide-line">
        {rows.map(({ o, edit, on, staged }) => (
          <li key={o.name} className="flex flex-wrap items-center gap-x-3 gap-y-0.5 py-1.5 sm:grid sm:grid-cols-[minmax(0,12rem)_minmax(0,1fr)_auto]">
            <span className={cn("min-w-0 flex-1 truncate font-mono text-xs sm:flex-none", o.owner_settable ? "text-chalk" : "text-chalk-dim")}>{escapeName(o.name)}</span>
            <span
              className={cn("order-last w-full min-w-0 text-xs [overflow-wrap:anywhere] sm:order-none sm:w-auto sm:text-right", staged ? "text-sand" : on ? "text-seafoam" : "text-chalk-dim")}
              title={!staged && o.all_rows ? `on: policy ${escapeName(o.all_rows.policy)}` : undefined}
            >
              {staged ? (on ? "on, staged" : "off, staged") : o.all_rows ? `on: policy ${escapeName(o.all_rows.policy)}` : o.owner_settable ? "off" : `off · ${ownedBy(o.owner)}`}
            </span>
            <RoleSwitch
              label={`All rows on ${escapeName(o.name)}`}
              checked={on}
              staged={staged}
              disabled={!editable || !o.owner_settable}
              onChange={() => editor.stage([{ ...edit, enabled: !on } as Edit])}
            />
          </li>
        ))}
      </ul>
      <p className="mt-2 text-[13px] text-chalk-dim">
        Restrictive policies still apply, and a role without SELECT on a table sees nothing there.{" "}
        {admin && (
          <button
            type="button"
            disabled={!editable || off.length === 0}
            onClick={() => editor.stage(off.map((r) => ({ ...r.edit, enabled: true }) as Edit))}
            className="text-chalk underline underline-offset-4 outline-none hover:text-sand focus-visible:outline-2 focus-visible:outline-sand disabled:opacity-50"
          >
            All rows on every RLS table in {escapeName(schema)}
          </button>
        )}
      </p>
    </Block>
  );
}

function Sequences({ objects, editor, editable, admin }: Ctx & { objects: ObjectPrivileges[] }) {
  if (objects.length === 0) return null;
  const offered = (editor.head.data?.offered.sequence ?? []).map(upper);
  return (
    <Block title="Sequences">
      <ul className="space-y-1.5">
        {objects.map((o) => {
          const target: EditTarget = { kind: "sequence", schema: o.schema, name: o.name };
          return (
            <li key={o.name} className="flex flex-wrap items-center gap-x-5 gap-y-1">
              <span className={cn("flex w-48 items-center gap-1.5 truncate font-mono text-xs", o.owner_settable ? "text-chalk" : "text-chalk-dim")}>
                {!o.owner_settable && <Lock aria-hidden="true" size={12} strokeWidth={1.5} />}
                {escapeName(o.name)}
              </span>
              {offered.map((p) => (
                <PrivilegeBox key={p} editor={editor} edit={cell(target, p, o.grants.some((g) => g.privilege === p))} label={`${p} on ${escapeName(o.name)}`} text={p} disabled={!editable || !o.owner_settable} />
              ))}
              <Notes editor={editor} target={target} grants={o.grants} privileges={offered} owner={o.owner} settable={o.owner_settable} admin={admin} editable={editable} />
            </li>
          );
        })}
      </ul>
    </Block>
  );
}

function Functions({ objects, editor, editable, admin }: Ctx & { objects: ObjectPrivileges[] }) {
  if (objects.length === 0) return null;
  return (
    <Block title="Functions and procedures">
      <ul className="space-y-1.5">
        {objects.map((o) => {
          const identity = o.identity ?? `${o.schema}.${o.name}`;
          const target: EditTarget = { kind: "function", schema: o.schema, oid: o.oid, identity };
          return (
            <li key={o.oid} className="flex flex-wrap items-center gap-x-5 gap-y-1">
              <span className={cn("flex min-w-72 items-center gap-1.5 font-mono text-xs break-all", o.owner_settable ? "text-chalk" : "text-chalk-dim")}>
                {!o.owner_settable && <Lock aria-hidden="true" size={12} strokeWidth={1.5} />}
                <span>{escapeName(identity)}</span>
                {o.kind === "procedure" && <Tag>procedure</Tag>}
              </span>
              <PrivilegeBox editor={editor} edit={cell(target, "EXECUTE", o.grants.some((g) => g.privilege === "EXECUTE"))} label={`EXECUTE on ${escapeName(identity)}`} text="EXECUTE" disabled={!editable || !o.owner_settable} />
              <Notes editor={editor} target={target} grants={o.grants} privileges={["EXECUTE"]} owner={o.owner} settable={o.owner_settable} admin={admin} editable={editable} />
            </li>
          );
        })}
      </ul>
    </Block>
  );
}

/** Static class names (Tailwind cannot see a built one): the label spans the rows its boxes wrap onto from sm. */
const ROW_SPAN: Record<number, string> = { 1: "sm:row-span-1", 2: "sm:row-span-2", 3: "sm:row-span-3", 4: "sm:row-span-4" };

const FUTURE: { object: DefaultObject; label: string; offered: "table" | "sequence" | "function" }[] = [
  { object: "tables", label: "New tables", offered: "table" },
  { object: "sequences", label: "New sequences", offered: "sequence" },
  { object: "functions", label: "New functions", offered: "function" },
];

/**
 * Default privileges for objects created later in the schema, `FOR ROLE` the creator chosen: the
 * database owner first, then the roles that own objects here and can be acted as (D6); never a
 * role the owner can't SET to.
 */
function FutureObjects({ schema, objects, editor, editable, dbOwner }: Ctx & { schema: SchemaPrivileges; objects: ObjectPrivileges[]; dbOwner: string | null }) {
  const selectId = useId();
  const owners = [
    ...(dbOwner ? [dbOwner] : []),
    ...(schema.owner_settable ? [schema.owner] : []),
    ...objects.filter((o) => o.owner_settable).map((o) => o.owner),
  ].filter((v, i, all) => all.indexOf(v) === i);
  const [forRole, setForRole] = useState<string | null>(null);
  const creator = forRole !== null && owners.includes(forRole) ? forRole : owners[0];
  const offered = editor.head.data?.offered;
  if (!creator || !offered) return null;
  return (
    <Block title="Future objects" description={`Default privileges for objects created later in ${escapeName(schema.name)}.`}>
      <div className="mb-2.5 flex flex-wrap items-center gap-2.5">
        <label htmlFor={selectId} className="text-[13px] text-chalk-dim">
          Created by
        </label>
        <Select
          id={selectId}
          value={creator}
          onChange={(e) => setForRole(e.target.value)}
          wrapperClassName="w-56" className="font-mono text-xs"
        >
          {owners.map((o) => (
            <option key={o} value={o}>
              {o === dbOwner ? `${escapeName(o)} (owner)` : escapeName(o)}
            </option>
          ))}
        </Select>
      </div>
      <ul className="space-y-1.5">
        {FUTURE.map((f) => {
          const target: EditTarget = { kind: "default", forRole: creator, schema: schema.name, object: f.object };
          const entry = editor.head.data?.default_privileges.find((d) => d.for_role === creator && d.schema === schema.name && d.object === f.object);
          return (
            <li key={f.object} className="grid grid-cols-2 items-center gap-x-3 gap-y-1 sm:grid-cols-[6.5rem_repeat(4,8rem)]">
              {/* One fixed column count on every row, so the boxes line up; the label spans the rows its boxes wrap onto. */}
              <span className={cn("col-span-2 self-start pt-1.5 text-[13px] text-chalk sm:col-span-1", ROW_SPAN[Math.min(4, Math.max(1, Math.ceil(offered[f.offered].length / 4)))])}>
                {f.label}
              </span>
              {offered[f.offered].map(upper).map((p) => (
                <PrivilegeBox
                  key={p}
                  editor={editor}
                  edit={cell(target, p, (entry?.grants ?? []).some((g) => g.privilege === p))}
                  label={`${p} on ${f.label.toLowerCase()}`}
                  text={p}
                  disabled={!editable}
                />
              ))}
            </li>
          );
        })}
      </ul>
    </Block>
  );
}
