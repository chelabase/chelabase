import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";

export type HeldStatement = { index: number; kind: string; text_preview: string; reason: "risky" | "protected" | string };

export interface SqlConfirmDialogProps {
  statements: HeldStatement[] | null;
  onConfirm: () => void;
  onCancel: () => void;
}

const REASON: Record<string, string> = { risky: "risky", protected: "protected branch" };

/** Opened by the server's 409 `sql_confirm_required`: lists what it held back. */
export function SqlConfirmDialog({ statements, onConfirm, onCancel }: SqlConfirmDialogProps) {
  return (
    <Modal open={statements !== null} onOpenChange={(o) => !o && onCancel()} title="Run these statements?">
      <p className="text-sm">These statements need your confirmation before they run.</p>
      <ul className="max-h-60 space-y-2 overflow-auto text-sm">
        {(statements ?? []).map((s) => (
          <li key={s.index} className="rounded border p-2">
            <p className="text-xs text-muted-foreground">
              {s.kind} · {REASON[s.reason] ?? s.reason}
            </p>
            <pre className="font-mono text-xs whitespace-pre-wrap">{s.text_preview}</pre>
          </li>
        ))}
      </ul>
      <div className="flex justify-end gap-2">
        <Button variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button onClick={onConfirm}>Confirm</Button>
      </div>
    </Modal>
  );
}
