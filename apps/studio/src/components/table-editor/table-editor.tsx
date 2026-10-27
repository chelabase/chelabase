import { useEffect, useLayoutEffect, useRef, useState, type ComponentType, type ReactNode } from "react";
import { Tabs as TabsPrimitive } from "@base-ui/react/tabs";
import { useQueryClient } from "@tanstack/react-query";
import {
  useGetDatabase,
  useGetMe,
  useListBranchDatabases,
  useListBranches,
  useListObjects,
  useListProjects,
  useListRoles,
  type Branch,
  type ObjectSummary,
  type Project,
} from "@chelabase/management-client";
import { useCrumbLabel } from "@/components/app-shell/crumbs";
import { HeaderContext } from "@/components/app-shell/header-context";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { DATA_API_ROLES, actAsRoles, actingCaps, dataApiActAs, projectCaps, type ProjectCaps } from "@/lib/capabilities";
import { isTransitional } from "@/lib/branches";
import { pageSizeFor } from "@/lib/console";
import { pollWhile } from "@/lib/query";
import { errorText } from "@/lib/errors";
import { useRealtime } from "@/lib/realtime/context";
import { fallbackInterval, invalidateAll, invalidateForEvent } from "@/lib/realtime/events";
import { rowAccess, structureAccess } from "@/lib/table-editor/access";
import { changesetKey } from "@/lib/table-editor/changeset";
import { branchTree, editorPath, readLast, resolveParams, writeLast, type EditorParams, type Resolution } from "@/lib/table-editor/context";
import { formatBytes } from "@/lib/table-editor/dry-run";
import { TableEditorStoreContext, createTableEditorStore, useTableEditor } from "@/lib/table-editor/store";
import { ColumnStrip } from "./column-strip";
import { ContextBar } from "./context-bar";
import { DiscardPrompt, EditorPanel, reviewLabel } from "./panel";
import { RlsBanner } from "./rls-banner";
import { RowsView } from "./rows-view";
import { NewTableView } from "./new-table";
import { StructureView } from "./structure-view";
import { TableList } from "./table-list";
import { TypesView } from "./types-view";
import type { TableTarget } from "./target";

export interface TableEditorProps {
  params: EditorParams;
  navigate: (p: EditorParams, opts?: { replace?: boolean }) => void;
  /** Rendered inside the editor's store (the route's blocker for a dirty row draft); the editor stays router-free. */
  leaveGuard?: ComponentType;
}

const NOT_FOUND: Record<Extract<Resolution, { kind: "not_found" }>["level"], string> = {
  project: "Project",
  database: "Database",
  branch: "Branch",
  pgDatabase: "Postgres database",
  schema: "Schema",
  table: "Table",
};

/**
 * The projects with their databases. `/v1/projects` lists the projects; the caller's databases
 * per project come with `/v1/me` (`tenant.projects`). Absent until both have loaded.
 */
function withDatabases(list: ProjectInfoLike[] | undefined, me: { tenant: { projects: Project[] } | null } | undefined): Project[] | undefined {
  if (!list || !me) return undefined;
  const mine = new Map((me.tenant?.projects ?? []).map((p) => [p.id, p]));
  return list.map((p) => ({ ...p, databases: mine.get(p.id)?.databases ?? p.databases ?? [] }) as Project);
}
type ProjectInfoLike = { id: string; name: string; slug: string; status: Project["status"]; databases?: Project["databases"] };

/**
 * Act as's roles with the data API's last (the default stays the first role of the caller's own):
 * `anon` and `authenticated` only when the grant names them (or a granted role's chain reaches
 * them), even when the branch's role list doesn't show them (R18: an Admin's every role doesn't
 * hold them).
 */
function withDataApiRoles(caps: ProjectCaps, allowed: { owner: boolean; roles: string[] }) {
  const api: string[] = DATA_API_ROLES.filter((r) => dataApiActAs(caps).includes(r) || allowed.roles.includes(r));
  return { owner: allowed.owner, roles: [...allowed.roles.filter((r) => !api.includes(r)), ...api] };
}

function browserStorage(kind: "local" | "session"): Storage | null {
  try {
    return kind === "local" ? window.localStorage : window.sessionStorage;
  } catch {
    return null;
  }
}

/**
 * Studio's table editor: the context bar in the header, the table list, and the main area.
 * Router-free: the route passes the URL's params and a `navigate`.
 */
export function TableEditor(props: TableEditorProps) {
  const [store] = useState(() => createTableEditorStore(browserStorage("session")));
  return (
    <TableEditorStoreContext.Provider value={store}>
      {props.leaveGuard && <props.leaveGuard />}
      <Editor {...props} />
    </TableEditorStoreContext.Provider>
  );
}

/**
 * The height that fills the viewport below the element's top (the shell header's height isn't
 * hard-coded here): measured on mount and on resize.
 */
function useFillViewport() {
  const ref = useRef<HTMLDivElement>(null);
  const [top, setTop] = useState<number | null>(null);
  useLayoutEffect(() => {
    const measure = () => {
      const el = ref.current;
      if (el) setTop(Math.max(0, el.getBoundingClientRect().top + window.scrollY));
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, []);
  return { ref, style: top === null ? undefined : { height: `calc(100dvh - ${top}px)` } };
}

/**
 * The project's `project:` channel (§5.3): its events invalidate exactly the queries they concern;
 * a rejoin after a reconnect, or a `resync`, invalidates everything the editor shows. True while joined.
 */
function useProjectChannel(projectId: string | undefined, databaseId: string | undefined, branchId: string | undefined): boolean {
  const realtime = useRealtime();
  const queryClient = useQueryClient();
  const [live, setLive] = useState(false);
  const scope = useRef({ databaseId: "", branchId: "" });
  scope.current = { databaseId: databaseId ?? "", branchId: branchId ?? "" };
  useEffect(() => {
    if (!realtime || !projectId) return;
    let joinedBefore = false;
    const sub = realtime.joinProject(projectId, {
      onEvent: (event) => invalidateForEvent(queryClient, event),
      onResync: () => invalidateAll(queryClient, scope.current),
      onLive: (next) => {
        setLive(next);
        if (!next) return;
        // Events may have been missed while the socket was down.
        if (joinedBefore) invalidateAll(queryClient, scope.current);
        joinedBefore = true;
      },
      onError: () => setLive(false),
    });
    return () => {
      sub.leave();
      setLive(false);
    };
  }, [realtime, projectId, queryClient]);
  return live;
}

function Editor({ params, navigate }: TableEditorProps) {
  const fill = useFillViewport();
  // The last place, read once: it only matters for the levels this visit leaves out.
  const [last] = useState(() => readLast(browserStorage("local")));
  // P10: while the project channel is live, events drive the refetches and a slow poll is the fallback.
  const projectLive = useProjectChannel(params.projectId, params.databaseId, params.branchId);
  const me = useGetMe({ query: { refetchInterval: fallbackInterval(projectLive, false) } });
  const projectList = useListProjects();
  const projects = withDatabases(projectList.data, me.data);
  const project = projects?.find((p) => p.id === params.projectId);
  const databaseOk = !!params.databaseId && !!project?.databases.some((d) => d.database_id === params.databaseId);
  const databaseId = databaseOk ? params.databaseId! : "";
  const detail = useGetDatabase(databaseId, { query: { enabled: databaseOk } });
  // Only a branch created here is polled until it settles; opening the Branch picker refetches the list.
  // While live, branch events invalidate the list instead (the slow poll stays as the fallback).
  const [createdBranchId, setCreatedBranchId] = useState<string | null>(null);
  const settling = pollWhile<Branch[]>((bs) => bs.some((b) => b.id === createdBranchId && isTransitional(b)));
  const branches = useListBranches(databaseId, {
    query: {
      enabled: databaseOk,
      refetchInterval: (query) => fallbackInterval(projectLive, settling(query)),
    },
  });
  const branch = branches.data?.find((b) => b.id === params.branchId);
  const branchOk = databaseOk && !!branch && branchTree(branches.data ?? []).some((r) => r.branch.id === branch.id);
  const branchId = branchOk ? params.branchId! : "";
  const pgDatabases = useListBranchDatabases(databaseId, branchId, { query: { enabled: branchOk } });
  // The system database (`postgres`) is the platform's: the table API refuses it, so it isn't offered (M2).
  const pgList = pgDatabases.data?.filter((d) => d.origin !== "system");
  const pg = pgList?.find((d) => d.name === params.pgDatabase);
  // The branch's main database is addressed without a name (P9).
  const pgParam = pg && !pg.main ? pg.name : undefined;
  const objects = useListObjects(databaseId, branchId, pgParam ? { database: pgParam } : undefined, {
    query: { enabled: branchOk && !!pg },
  });
  const roles = useListRoles(databaseId, branchId, { query: { enabled: branchOk } });

  const resolution = resolveParams(params, last, {
    projects,
    branches: databaseOk ? branches.data : undefined,
    pgDatabases: branchOk ? pgList : undefined,
    objects: branchOk && pg ? objects.data?.objects : undefined,
  });
  const current = resolution.kind === "redirect" || resolution.kind === "ready" ? resolution.params : params;
  const currentPath = editorPath(current);
  const redirect = resolution.kind === "redirect";
  const ready = resolution.kind === "ready";
  useEffect(() => {
    if (redirect) navigate(current, { replace: true });
    // `current` is determined by `currentPath`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [redirect, currentPath]);
  useEffect(() => {
    if (ready) writeLast(browserStorage("local"), current);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready, currentPath]);

  const caps = projectCaps(me.data, params.projectId ?? "", params.databaseId);
  const actAsKey = `${params.databaseId}/${params.branchId}`;
  const picked = useTableEditor((s) => s.actAs[actAsKey]);
  const setActAs = useTableEditor((s) => s.setActAs);
  // Leaving the table (another table, branch, database, project, an FK link) or switching Act as remounts the row
  // panel: an unsaved draft is asked about first.
  const guardDiscard = useTableEditor((s) => s.guardDiscard);
  const go = (p: EditorParams) => guardDiscard(() => navigate(p));
  // A roles error degrades to no roles (the owner stays for an Admin); the error shows by Act as.
  const allowed = withDataApiRoles(caps, actAsRoles(caps, roles.isSuccess ? roles.data : []));
  // The chosen role while the caller may still act as it, else the owner (Admin) or the first allowed role.
  const chosenOk = picked === null ? allowed.owner : picked !== undefined && allowed.roles.includes(picked);
  const actAs = chosenOk ? picked! : allowed.owner ? null : (allowed.roles[0] ?? null);
  const actAsReady = branchOk && me.isSuccess && (roles.isSuccess || roles.isError);
  const rolesError = branchOk && roles.isError ? errorText(roles.error) : null;

  const error = projectList.error ?? me.error ?? branches.error ?? pgDatabases.error ?? objects.error;
  // Write controls follow the level of the role acted as (D1), not the project's alone.
  const actedCaps = actingCaps(caps, actAs, roles.isSuccess ? roles.data : []);
  const structure = branch ? structureAccess(actedCaps, branch) : null;
  const structureReason = structure && !structure.allowed ? structure.reason : null;
  // Dry runs and previews are sent as the act-as role: none goes out before that role is known
  // (it would go as the owner), nor when there is no role to act as.
  const dryRunReason =
    structureReason ??
    (!actAsReady ? "Loading roles…" : !allowed.owner && actAs === null ? "You can't act as any Postgres role here." : null);
  const rows = pg ? rowAccess(actedCaps, { owner: allowed.owner && actAs === null, roles: actAs ? [actAs] : [] }, pg) : null;
  const object = objects.data?.objects?.find((o) => o.schema === current.schema && o.name === current.table);
  useCrumbLabel("table", object ? current.table : undefined, current.table);
  const target: TableTarget | null =
    ready && current.table && pg && current.schema
      ? {
          databaseId: current.databaseId!,
          branchId: current.branchId!,
          pgDatabase: pg.name,
          database: pgParam ?? null,
          schema: current.schema,
          table: current.table,
          role: actAs,
          key: changesetKey({ databaseId: current.databaseId!, branchId: current.branchId!, pgDatabase: pg.name, schema: current.schema, table: current.table }),
          ...(object && !(object.kind === "table" && object.editable) ? { notTable: true } : {}),
        }
      : null;
  // The enums view and the new-table form have no table: their changesets are `…/{schema}.#types` and `…/{schema}.#new`.
  const view = useTableEditor((s) => s.view);
  const setView = useTableEditor((s) => s.setView);
  const draft = view === "types" ? ("types" as const) : view === "new_table" ? ("new_table" as const) : null;
  const draftTarget: TableTarget | null =
    ready && draft && pg && current.schema
      ? {
          databaseId: current.databaseId!,
          branchId: current.branchId!,
          pgDatabase: pg.name,
          database: pgParam ?? null,
          schema: current.schema,
          table: draft === "types" ? "#types" : "#new",
          role: actAs,
          key: changesetKey({ databaseId: current.databaseId!, branchId: current.branchId!, pgDatabase: pg.name, schema: current.schema, table: draft === "types" ? "#types" : "#new" }),
          draft,
        }
      : null;
  // Another table (or database, or branch) shows the table view again.
  // A schema switch leaves Types and New table too (neither view is in the URL).
  const viewScope = `${current.databaseId}/${current.branchId}/${current.pgDatabase}/${current.schema ?? ""}/${current.table ?? ""}`;
  const lastScope = useRef(viewScope);
  useEffect(() => {
    if (lastScope.current !== viewScope) setView("table");
    lastScope.current = viewScope;
  }, [viewScope, setView]);
  const pageSize = detail.isPending ? null : pageSizeFor(detail.data?.max_rows);
  const exactRows = useTableEditor((s) => (target ? (s.exactRows[target.key] ?? null) : null));

  let main;
  if (resolution.kind === "not_found") {
    main = <p className="p-6 text-sm">{NOT_FOUND[resolution.level]} not found</p>;
  } else if (error) {
    main = (
      <p role="alert" className="p-6 text-sm text-coral">
        {errorText(error)}
      </p>
    );
  } else if (!ready) {
    main = (
      <p role="status" className="p-6 text-sm text-chalk-dim">
        Loading…
      </p>
    );
  } else if (draftTarget) {
    const reason = structure && !structure.allowed ? structure.reason : null;
    main = draftTarget.draft === "types" ? <TypesView key={draftTarget.key} target={draftTarget} reason={reason} /> : <NewTableView key={draftTarget.key} target={draftTarget} reason={reason} />;
  } else if (!current.projectId) {
    main = <p className="p-6 text-sm text-chalk-dim">No projects yet</p>;
  } else if (!current.databaseId) {
    main = <p className="p-6 text-sm text-chalk-dim">This project has no databases</p>;
  } else if (!current.branchId) {
    main = <p className="p-6 text-sm text-chalk-dim">No ready branch</p>;
  } else if (!current.pgDatabase) {
    main = <p className="p-6 text-sm text-chalk-dim">This branch has no main Postgres database: pick one above</p>;
  } else if (!current.table || !target || !pg) {
    main = (
      <>
        {structureReason && (
          <p data-testid="structure-reason" className="border-b border-line px-4 py-2 text-sm text-chalk-dim">
            {structureReason}
          </p>
        )}
        <p className="p-6 text-sm text-chalk-dim">Pick a table</p>
      </>
    );
  } else {
    main = (
      <TableView
        key={target.key}
        target={target}
        object={object}
        structureReason={structure && !structure.allowed ? structure.reason : null}
        renderRows={(toolsSlot) =>
          actAsReady && rows ? (
            <RowsView
              target={target}
              access={rows}
              pg={pg}
              canEnableDataApi={caps.atLeast("developer")}
              object={object}
              pageSize={pageSize}
              toolsSlot={toolsSlot}
              selfSub={me.data?.sub}
              above={(layout) => <ColumnStrip target={target} layout={layout} reason={structure && !structure.allowed ? structure.reason : null} />}
            />
          ) : (
            <p role="status" className="p-6 text-sm text-chalk-dim">
              Loading…
            </p>
          )
        }
      />
    );
  }

  return (
    <div ref={fill.ref} style={fill.style} className="flex min-h-0 max-[40rem]:flex-col">
      <HeaderContext>
        <ContextBar
          params={current}
          navigate={go}
          projects={projects}
          branches={databaseOk ? branches.data : undefined}
          pgDatabases={branchOk ? pgList : undefined}
          actAs={actAsReady ? { owner: allowed.owner, roles: allowed.roles, selected: actAs } : undefined}
          onActAs={(role) => guardDiscard(() => setActAs(actAsKey, role))}
          rolesError={rolesError}
          historyWindow={detail.data?.history_window}
          onBranchesOpen={() => void branches.refetch()}
          onBranchCreated={(b) => setCreatedBranchId(b.id)}
        />
      </HeaderContext>
      <TableList
        params={current}
        navigate={go}
        objects={branchOk && pg ? objects.data?.objects : undefined}
        noDatabase={ready && !current.pgDatabase}
        exactRows={exactRows}
      />
      <div className="flex min-w-0 flex-1 flex-col max-[40rem]:min-h-[24rem]">{main}</div>
      <EditorPanel
        target={draftTarget ?? target}
        access={draftTarget ? null : rows}
        object={draftTarget ? undefined : object}
        params={current}
        navigate={go}
        structureReason={dryRunReason}
        totalBytes={draftTarget ? null : (object?.total_bytes ?? null)}
      />
      <DiscardPrompt />
    </div>
  );
}

type TableTab = "rows" | "structure";

interface TableViewProps {
  target: TableTarget;
  object: ObjectSummary | undefined;
  /** Why structure can't be changed here (protected branch, Viewer), or null. */
  structureReason: string | null;
  /** The Rows tab, given where its tools go in the table bar. */
  /** `toolsSlot` is null while another tab is open (the tools then stay inside the hidden panel). */
  renderRows: (toolsSlot: HTMLElement | null) => ReactNode;
}

/** The open table: its bar (name, facts, Rows | Structure, the view's tools) over the current tab. */
function TableView({ target, object, structureReason, renderRows }: TableViewProps) {
  const [tab, setTab] = useState<TableTab>("rows");
  const pending = useTableEditor((s) => s.changesets[target.key]?.edits.length ?? 0);
  const panel = useTableEditor((s) => s.panel);
  const openPanel = useTableEditor((s) => s.openPanel);
  const setCollapsed = useTableEditor((s) => s.setCollapsed);
  const exactRows = useTableEditor((s) => s.exactRows[target.key] ?? null);
  // The rows on screen: the estimate never reads below them.
  const shownRows = useTableEditor((s) => s.shownRows[target.key] ?? 0);
  // Structure has no use for the open row: its panel folds to the strip (its draft kept) and Rows unfolds it.
  const foldedRow = useRef(false);
  function changeTab(next: TableTab) {
    if (next === "structure" && panel.kind === "row" && !panel.collapsed) {
      setCollapsed(true);
      foldedRow.current = true;
    } else if (next === "rows") {
      if (foldedRow.current && panel.kind === "row" && panel.collapsed) setCollapsed(false);
      foldedRow.current = false;
    }
    setTab(next);
  }
  const applied = useTableEditor((s) => (s.applied?.key === target.key ? s.applied.text : null));
  const statusRef = useRef<HTMLParagraphElement>(null);
  useEffect(() => {
    const active = document.activeElement;
    if (applied && (!active || active === document.body || !active.isConnected)) statusRef.current?.focus();
  }, [applied]);
  // The changes list is out of sight while another panel is open or the panel is collapsed.
  const reviewHidden = pending > 0 && (panel.kind === "column" || panel.kind === "row" || panel.collapsed);
  const isTable = !object || (object.kind === "table" && object.editable);
  const [toolsSlot, setToolsSlot] = useState<HTMLDivElement | null>(null);
  const facts = object
    ? [
        object.row_estimate != null
          ? `≈ ${Math.max(object.row_estimate, shownRows).toLocaleString("en-US")} rows`
          : exactRows !== null
            ? `${exactRows.toLocaleString("en-US")} ${exactRows === 1 ? "row" : "rows"}`
            : null,
        formatBytes(object.total_bytes),
        object.kind === "table" ? `RLS ${object.rls_enabled ? "on" : "off"}` : object.kind.replace("_", " "),
        `data API ${object.data_api_exposed ? "on" : "off"}`,
      ].filter((f): f is string => f !== null)
    : [];
  return (
    <TabsPrimitive.Root value={tab} onValueChange={(next) => changeTab(next as TableTab)} className="flex min-h-0 flex-1 flex-col">
      {/* The tools wrap to a second line before the name gives way; the name truncates within its share. */}
      <div className="flex shrink-0 flex-wrap items-start gap-x-4 gap-y-2 border-b border-line px-4 pt-3 pb-3">
        <div className="max-w-[45%] min-w-0 shrink-0">
          <h2 className="truncate font-mono text-lg leading-6 font-medium" title={`${target.schema}.${target.table}`}>
            {target.schema}.{target.table}
          </h2>
          {facts.length > 0 && <p className="mt-1 truncate text-[13px] text-chalk-dim">{facts.join(" · ")}</p>}
          {/* A live region from the start, so the Apply outcome is announced; it takes the focus Apply leaves.
              Its own line under the facts (empty, it takes no room). */}
          <p ref={statusRef} role="status" tabIndex={-1} className={cn("flex items-center gap-2 text-[13px] text-chalk outline-none", applied && "mt-1")}>
            {applied && (
              <>
                <span aria-hidden="true" className="size-2 shrink-0 bg-seafoam" />
                <span className="truncate">{applied}</span>
              </>
            )}
          </p>
        </div>
        <TabsPrimitive.List aria-label="Table views" className="mt-0.5 flex border border-input">
          {(["rows", "structure"] as const).map((id) => (
            <TabsPrimitive.Tab
              key={id}
              value={id}
              className="px-3 py-2 text-sm text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand data-[active]:bg-channel data-[active]:text-chalk data-[active]:shadow-[inset_0_-2px_0_var(--sand)]"
            >
              {id === "rows" ? "Rows" : "Structure"}
            </TabsPrimitive.Tab>
          ))}
        </TabsPrimitive.List>
        {reviewHidden && (
          <Button size="sm" variant="outline" className="mt-1" onClick={() => openPanel("changes")}>
            {reviewLabel(pending)}
          </Button>
        )}
        {/* The current view's tools (Filter, Insert row) are portalled here. */}
        <div ref={setToolsSlot} className="ml-auto flex flex-wrap items-center gap-2 pt-0.5" />
      </div>
      {structureReason && (
        <p data-testid="structure-reason" className="border-b border-line px-4 py-2 text-sm text-chalk-dim">
          {structureReason}
        </p>
      )}
      {isTable && <RlsBanner target={target} reason={structureReason} enabled={isTable} />}
      {/* Kept mounted: the Rows view keeps its page, filter, sort and widths across a visit to Structure.
          Its tools sit in the bar only while it is the open tab. */}
      <TabsPrimitive.Panel value="rows" keepMounted className="flex min-h-0 flex-1 flex-col outline-none [&[hidden]]:hidden">
        {toolsSlot && renderRows(tab === "rows" ? toolsSlot : null)}
      </TabsPrimitive.Panel>
      <TabsPrimitive.Panel value="structure" className="flex min-h-0 flex-1 flex-col outline-none">
        <StructureView target={target} reason={structureReason} kind={object?.kind} />
      </TabsPrimitive.Panel>
    </TabsPrimitive.Root>
  );
}
