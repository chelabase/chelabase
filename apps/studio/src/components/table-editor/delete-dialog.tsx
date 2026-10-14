import { useId, useState } from "react";
import { Modal } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";

/** `all_matching`'s `count`: a filtered view's exact matching count when it is known, else null. */
export type DeleteTarget = { kind: "rows"; count: number } | { kind: "all_matching"; filtered: boolean; count: number | null };

export interface DeleteDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  target: DeleteTarget;
  /** The table's name: typed to confirm deleting every row. */
  table: string;
  /** Acting as the owner: row-level security doesn't narrow the delete. */
  asOwner: boolean;
  busy: boolean;
  error: string | null;
  onConfirm: () => void;
}

const plural = (n: number) => `${n} ${n === 1 ? "row" : "rows"}`;

/**
 * Confirms a delete: the selected rows by count, or every matching row; deleting every row of an
 * unfiltered table needs the table's name typed.
 */
export function DeleteDialog({ open, onOpenChange, target, table, asOwner, busy, error, onConfirm }: DeleteDialogProps) {
  const [typed, setTyped] = useState("");
  const inputId = useId();
  const all = target.kind === "all_matching";
  const needsName = all && !target.filtered;
  // A filtered delete names its count when it is known (M3); "all rows" is only the whole table.
  const matching = all && target.filtered && target.count !== null ? `${target.count.toLocaleString("en-US")} matching ${target.count === 1 ? "row" : "rows"}` : null;
  const title = !all ? `Delete ${plural(target.count)}?` : matching ? `Delete ${matching}?` : "Delete all matching rows?";
  const confirm = !all ? `Delete ${plural(target.count)}` : matching ? `Delete ${matching}` : target.filtered ? "Delete matching rows" : "Delete all rows";
  const ready = !needsName || typed === table;
  return (
    <Modal
      open={open}
      onOpenChange={(next) => {
        if (busy) return;
        if (!next) setTyped("");
        onOpenChange(next);
      }}
      title={title}
      description={
        all
          ? target.filtered
            ? "Every row that matches the current filter is deleted, not only the loaded ones. This can't be undone."
            : `Every row of ${table} is deleted. This can't be undone.`
          : "This can't be undone."
      }
    >
      {asOwner && <p className="text-sm text-chalk-dim">You act as the owner, so row-level security doesn&apos;t limit this.</p>}
      {needsName && (
        <div className="space-y-1">
          <label htmlFor={inputId} className="text-sm">
            Type {table} to confirm
          </label>
          <input
            id={inputId}
            value={typed}
            autoComplete="off"
            spellCheck={false}
            onChange={(e) => setTyped(e.target.value)}
            className="h-9 w-full border border-input bg-ink px-2 font-mono text-[13px] text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
          />
        </div>
      )}
      {error && (
        <p role="alert" className="text-sm text-coral">
          {error}
        </p>
      )}
      <div className="flex justify-end gap-2">
        <Button variant="ghost" disabled={busy} onClick={() => onOpenChange(false)}>
          Cancel
        </Button>
        <Button variant="destructive" disabled={busy || !ready} onClick={onConfirm}>
          {busy ? "Deleting…" : confirm}
        </Button>
      </div>
    </Modal>
  );
}
