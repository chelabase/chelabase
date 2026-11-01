import { Fragment, useCallback, useEffect, useRef, useState } from "react";
import type { Branch, BranchDatabaseView, Project } from "@chelabase/management-client";
import { Modal } from "@/components/ui/dialog";
import { NewBranchForm } from "@/components/new-branch-form";
import { relativeTime } from "@/lib/members";
import { branchTree, type EditorParams } from "@/lib/table-editor/context";
import { fadeMask, scrollFade, type ScrollFade } from "@/lib/scroll-fade";
import { Picker, type PickerOption } from "./picker";

const NEW_BRANCH = "\u0000new-branch";
const OWNER = "\u0000owner";

export interface ActAsChoice {
  /** The owner is among the choices (Admin). */
  owner: boolean;
  roles: string[];
  /** The role acted as now; null = the owner. */
  selected: string | null;
}

export interface ContextBarProps {
  params: EditorParams;
  /**
   * The deepest level the view works at (default `"table"`). `"pgDatabase"` stops at the Postgres
   * database: a view with no schema or table (the SQL editor) shows no Schema or Table level.
   */
  depth?: "pgDatabase" | "table";
  navigate: (p: EditorParams) => void;
  /** Each list is absent until it has loaded; its picker shows a placeholder until then. */
  projects?: Project[];
  branches?: Branch[];
  pgDatabases?: BranchDatabaseView[];
  /** Absent until the caller's roles and grants have loaded. */
  actAs?: ActAsChoice;
  onActAs: (role: string | null) => void;
  /** Why the branch's roles couldn't be loaded (shown by Act as), or null. */
  rolesError?: string | null;
  /** The database's history window, for the new-branch form; absent while the database detail loads. */
  historyWindow: number | undefined;
  /** The Branch picker was opened: the caller refreshes the list. */
  onBranchesOpen?: () => void;
  /** A branch was created from the picker's form. */
  onBranchCreated?: (branch: Branch) => void;
}

function Waiting({ label }: { label: string }) {
  return (
    <div aria-hidden="true" className="flex h-9 items-center gap-2 border border-line px-2 text-[11px] text-chalk-dim">
      {label}
    </div>
  );
}

const slash = (
  <span aria-hidden="true" className="text-chalk-dim">
    /
  </span>
);

/** The header's context bar: one labelled picker per level, and the role to act as. */
export function ContextBar({ params, navigate, projects, branches, pgDatabases, actAs, onActAs, rolesError = null, historyWindow, onBranchesOpen, onBranchCreated }: ContextBarProps) {
  const [newBranch, setNewBranch] = useState(false);
  const barRef = useRef<HTMLDivElement>(null);
  const [fade, setFade] = useState<ScrollFade>({ start: false, end: false });
  const measure = useCallback(() => {
    const el = barRef.current;
    if (!el) return;
    const next = scrollFade(el.scrollLeft, el.clientWidth, el.scrollWidth);
    setFade((prev) => (prev.start === next.start && prev.end === next.end ? prev : next));
  }, []);
  // The bar scrolls sideways when it doesn't fit: fade the edges that have more behind them.
  useEffect(() => {
    measure();
    const el = barRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  });
  const project = projects?.find((p) => p.id === params.projectId);
  const tree = branches ? branchTree(branches) : undefined;
  const branch = branches?.find((b) => b.id === params.branchId);

  const projectOptions: PickerOption[] = (projects ?? []).map((p) => ({ value: p.id, label: p.name }));
  const databaseOptions: PickerOption[] = (project?.databases ?? []).map((d) => ({ value: d.database_id, label: d.name }));
  const branchOptions: PickerOption[] = [
    ...(tree ?? []).map(({ branch: b, depth }) => ({
      value: b.id,
      label: b.name,
      depth,
      tag: b.protected ? "protected" : undefined,
      note: relativeTime(b.created_at),
    })),
    { value: NEW_BRANCH, label: `New branch from ${branch?.name ?? "…"}…`, action: true },
  ];
  const pgOptions: PickerOption[] = (pgDatabases ?? []).map((d) => ({
    value: d.name,
    label: d.name,
    tag: d.data_api === "on" ? "data API" : undefined,
  }));

  const actOptions: PickerOption[] = actAs
    ? [...(actAs.owner ? [{ value: OWNER, label: "owner" }] : []), ...actAs.roles.map((r) => ({ value: r, label: r }))]
    : [];
  const actValue = actAs ? (actAs.selected === null ? (actAs.owner ? OWNER : null) : actAs.selected) : null;

  return (
    // Narrow screens: the bar scrolls sideways rather than overflowing the header.
    <div
      ref={barRef}
      onScroll={measure}
      style={{ maskImage: fadeMask(fade), WebkitMaskImage: fadeMask(fade) }}
      className="flex min-w-0 flex-1 items-center gap-2 overflow-x-auto [&>*]:shrink-0 sm:[&>*]:shrink"
    >
      {projects ? (
        <Picker
          label="Project"
          options={projectOptions}
          value={params.projectId ?? null}
          onChange={(id) => navigate({ projectId: id })}
        />
      ) : (
        <Waiting label="Project" />
      )}
      {project && (
        <Fragment>
          {slash}
          <Picker
            label="Database"
            options={databaseOptions}
            value={params.databaseId ?? null}
            onChange={(id) => navigate({ projectId: params.projectId, databaseId: id })}
          />
        </Fragment>
      )}
      {project && params.databaseId && (
        <Fragment>
          {slash}
          {branches ? (
            <Picker
              label="Branch"
              mono
              options={branchOptions}
              value={params.branchId ?? null}
              onOpenChange={(open) => open && onBranchesOpen?.()}
              onChange={(id) => {
                if (id === NEW_BRANCH) setNewBranch(true);
                else navigate({ projectId: params.projectId, databaseId: params.databaseId, branchId: id });
              }}
            />
          ) : (
            <Waiting label="Branch" />
          )}
        </Fragment>
      )}
      {branch && (
        <Fragment>
          {slash}
          {pgDatabases ? (
            <Picker
              label="Postgres db"
              mono
              options={pgOptions}
              value={params.pgDatabase ?? null}
              onChange={(name) =>
                navigate({ projectId: params.projectId, databaseId: params.databaseId, branchId: params.branchId, pgDatabase: name })
              }
            />
          ) : (
            <Waiting label="Postgres db" />
          )}
        </Fragment>
      )}
      <span className="flex-1" />
      {rolesError !== null && (
        <p role="alert" className="shrink-0 text-[13px] whitespace-nowrap text-coral">
          Couldn&apos;t load roles: {rolesError}
        </p>
      )}
      {actAs && (
        <Picker
          label="Act as"
          mono
          options={actOptions}
          value={actValue}
          placeholder="No role"
          disabled={actOptions.length === 0}
          onChange={(v) => onActAs(v === OWNER ? null : v)}
        />
      )}
      {params.databaseId && branches && (
        <Modal
          open={newBranch}
          onOpenChange={setNewBranch}
          title={`New branch from ${branch?.name ?? "a branch"}`}
          description="The new branch starts as a copy of its parent."
          size="wide"
        >
          <NewBranchForm
            databaseId={params.databaseId}
            branches={branches}
            historyWindow={historyWindow}
            defaultParent={params.branchId}
            onCreated={(b) => {
              setNewBranch(false);
              onBranchCreated?.(b);
            }}
          />
        </Modal>
      )}
    </div>
  );
}
