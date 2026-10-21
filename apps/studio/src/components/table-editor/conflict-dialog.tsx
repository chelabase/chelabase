import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { CHANGED_TITLE, GONE_TEXT, NO_CHECK_NOTICE } from "@/lib/realtime/conflict";
import { cn } from "@/lib/utils";

export interface ConflictColumn {
  name: string;
  /** The row's value now (text; null = NULL). */
  theirs: string | null;
  /** The value being saved. */
  yours: string | null;
}

export interface ConflictDialogProps {
  open: boolean;
  /** `changed`: the row is there with other values; `gone`: the re-read found no row. */
  state: { kind: "changed"; columns: ConflictColumn[] } | { kind: "gone" };
  /** Edited columns the check left out; the notice "No conflict check for this column type" lists them. */
  unchecked: string[];
  busy?: boolean;
  error?: string | null;
  onOverwrite(): void;
  onTakeTheirs(): void;
  onClose(): void;
}

function Value({ text }: { text: string | null }) {
  return <span className={cn("font-mono text-xs break-all whitespace-pre-wrap", text === null && "text-chalk-dim")}>{text ?? "NULL"}</span>;
}

/**
 * A save that matched no row (§6.3): the row changed since the edit began (theirs against yours,
 * Overwrite or Take theirs), or it is gone for this role.
 */
export function ConflictDialog({ open, state, unchecked, busy = false, error, onOverwrite, onTakeTheirs, onClose }: ConflictDialogProps) {
  const changed = state.kind === "changed";
  return (
    <Modal
      open={open}
      onOpenChange={(next) => !next && !busy && onClose()}
      title={changed ? CHANGED_TITLE : "Not saved"}
      description={changed ? "The row changed after you started editing. Nothing was saved." : GONE_TEXT}
      size={changed ? "wide" : "default"}
    >
      {state.kind === "changed" && (
        <table className="w-full table-fixed border border-line text-sm">
          <thead className="bg-channel text-xs text-chalk-dim">
            <tr>
              <th scope="col" className="w-1/4 px-3 py-1.5 text-left font-normal">
                Column
              </th>
              <th scope="col" className="px-3 py-1.5 text-left font-normal">
                Theirs
              </th>
              <th scope="col" className="px-3 py-1.5 text-left font-normal">
                Yours
              </th>
            </tr>
          </thead>
          <tbody>
            {state.columns.map((c) => (
              <tr key={c.name} className="border-t border-line align-top">
                <th scope="row" className="truncate px-3 py-2 text-left font-mono text-xs font-normal">
                  {c.name}
                </th>
                <td className="px-3 py-2">
                  <Value text={c.theirs} />
                </td>
                <td className="px-3 py-2">
                  <Value text={c.yours} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {unchecked.length > 0 && (
        <p className="text-sm text-chalk-dim">
          {NO_CHECK_NOTICE}: <span className="font-mono text-xs">{unchecked.join(", ")}</span>
        </p>
      )}
      {error && (
        <p role="alert" className="text-sm text-coral">
          {error}
        </p>
      )}
      <div className="flex justify-end gap-2">
        {changed ? (
          <>
            <Button variant="ghost" disabled={busy} onClick={onTakeTheirs}>
              Take theirs
            </Button>
            <Button variant="destructive" disabled={busy} aria-busy={busy || undefined} onClick={onOverwrite}>
              Overwrite
            </Button>
          </>
        ) : (
          <Button variant="ghost" onClick={onClose}>
            Close
          </Button>
        )}
      </div>
    </Modal>
  );
}
