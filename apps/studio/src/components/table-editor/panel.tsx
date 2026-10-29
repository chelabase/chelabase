import { useEffect, useRef, type ComponentProps, type ReactNode } from "react";
import { Columns3, ListChecks } from "lucide-react";
import type { ObjectSummary } from "@chelabase/management-client";
import type { RowAccess } from "@/lib/table-editor/access";
import type { EditorParams } from "@/lib/table-editor/context";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { useTableEditor } from "@/lib/table-editor/store";
import { ChangesPanel } from "./changes-panel";
import { ColumnFormPanel } from "./column-form";
import { RowPanelHost } from "./row-panel";
import { changesWord, CollapseButton, CollapsedStrip } from "./panel-bits";
import type { TableTarget } from "./target";

export interface EditorPanelProps {
  /** The open table; null when none is (no table picked, or the context isn't resolved). */
  target: TableTarget | null;
  /** What the caller may do with the table's rows (null while it isn't known). */
  access: RowAccess | null;
  /** The table's entry in the object list. */
  object: ObjectSummary | undefined;
  params: EditorParams;
  navigate: (p: EditorParams) => void;
  /** Why structure can't be changed here (protected branch, Viewer), or null. */
  structureReason?: string | null;
  /** The table's size (`total_bytes`), for the rewrite warning. */
  totalBytes?: number | null;
}

/**
 * Asks before the page unloads (the browser's own prompt) while any table has pending changes (they live in this tab
 * only) or the row panel holds an unsaved draft.
 */
export function useUnloadGuard() {
  const pending = useTableEditor((s) => Object.keys(s.changesets).length > 0 || s.rowDraft !== null);
  useEffect(() => {
    if (!pending) return;
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [pending]);
}

/**
 * The editor's one right panel (changes, column form, row panel), mounted after the main area.
 * The changes view shows whenever the open table has pending changes and no other view is open.
 * Collapsing keeps the view mounted (a form keeps its draft) behind a 48px icon strip that names it.
 */
export function EditorPanel({ target, access, object, params, navigate, structureReason = null, totalBytes = null }: EditorPanelProps) {
  useUnloadGuard();
  const panel = useTableEditor((s) => s.panel);
  const openPanel = useTableEditor((s) => s.openPanel);
  const setView = useTableEditor((s) => s.setView);
  // A row belongs to its table: another table (or none) closes the row panel.
  const tableKey = target?.key ?? null;
  const previous = useRef(tableKey);
  useEffect(() => {
    if (previous.current !== tableKey && panel.kind === "row") openPanel("closed", undefined, { discard: true });
    previous.current = tableKey;
  }, [tableKey, panel.kind, openPanel]);
  const count = useTableEditor((s) => (target ? (s.changesets[target.key]?.edits.length ?? 0) : 0));
  if (!target) return null;

  const draft = target.draft;
  // After an Apply that created, renamed, moved or dropped the table: open it where it now is.
  const onApplied = (to: { schema: string; table: string | undefined }) => {
    if (draft) setView("table");
    navigate({ ...params, schema: to.schema, table: to.table });
  };
  if (draft) {
    // The enums view and the new-table form only have the changes list.
    if (count === 0) return null;
    const heading = `Pending changes · ${count}`;
    return (
      <PanelFrame key={`changes:${target.key}`} label="Pending changes" heading={heading} strip={{ text: heading, icon: ListChecks, badge: count }}>
        <ChangesPanel target={target} reason={structureReason} totalBytes={null} onApplied={onApplied} />
      </PanelFrame>
    );
  }

  if (panel.kind === "row") return access ? <RowPanelHost target={target} access={access} object={object} params={params} navigate={navigate} /> : null;

  if (panel.kind === "column") {
    const title = panel.column === null ? "New column" : `Edit column ${panel.column}`;
    return (
      <PanelFrame key={`column:${target.key}:${panel.column ?? ""}`} label={title} heading={title} strip={{ text: `${title} · draft, not added yet`, icon: Columns3 }}>
        <ColumnFormPanel target={target} column={panel.column} reason={structureReason} totalBytes={totalBytes} />
      </PanelFrame>
    );
  }

  if (count === 0) return null;
  const heading = `Pending changes · ${count}`;
  return (
    <PanelFrame key={`changes:${target.key}`} label="Pending changes" heading={heading} strip={{ text: heading, icon: ListChecks, badge: count }}>
      <ChangesPanel target={target} reason={structureReason} totalBytes={totalBytes} onApplied={onApplied} />
    </PanelFrame>
  );
}

/**
 * "Discard changes to row N?": asked before anything drops the row panel's unsaved draft (`guardDiscard`). Escape and
 * the dialog's own close keep editing.
 */
export function DiscardPrompt() {
  const ask = useTableEditor((s) => s.discardAsk);
  const answer = useTableEditor((s) => s.answerDiscard);
  return (
    <Modal open={ask !== null} onOpenChange={(open) => !open && answer(false)} title={ask?.title ?? ""} description="Your edits to this row haven't been saved.">
      <div className="flex justify-end gap-2">
        <Button variant="ghost" onClick={() => answer(false)}>
          Keep editing
        </Button>
        <Button variant="destructive" onClick={() => answer(true)}>
          Discard
        </Button>
      </div>
    </Modal>
  );
}

/** Count text for the table bar's way back to the changes list. */
export function reviewLabel(n: number): string {
  return `Review ${changesWord(n)}`;
}

function PanelFrame({ label, heading, strip, children }: { label: string; heading: string; strip: Omit<ComponentProps<typeof CollapsedStrip>, "onExpand">; children: ReactNode }) {
  const collapsed = useTableEditor((s) => s.panel.collapsed);
  const setCollapsed = useTableEditor((s) => s.setCollapsed);
  // Opened because the previous view went away under the focus ("Add to changes"): focus the heading.
  const headingRef = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    const active = document.activeElement;
    if (!active || active === document.body || !active.isConnected) headingRef.current?.focus();
  }, []);
  return (
    <>
      <aside
        aria-label={label}
        hidden={collapsed}
        className="flex w-[360px] shrink-0 flex-col gap-3 overflow-y-auto border-l border-line-strong bg-deep p-4"
      >
        <div className="flex items-center gap-3">
          <h2 ref={headingRef} tabIndex={-1} className="min-w-0 flex-1 truncate text-base leading-6 font-semibold outline-none">
            {heading}
          </h2>
          <CollapseButton onClick={() => setCollapsed(true)} />
        </div>
        {children}
      </aside>
      {collapsed && <CollapsedStrip {...strip} onExpand={() => setCollapsed(false)} />}
    </>
  );
}
