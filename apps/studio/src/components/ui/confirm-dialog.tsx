import { AlertDialog } from "@base-ui/react/alert-dialog";
import type { ReactNode, RefObject } from "react";
import { cn } from "@/lib/utils";
import { Button } from "./button";
import { backdrop, popupFrame, popupSurface } from "./dialog";

export interface ConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  body: ReactNode;
  /** The confirming button's label: names the action ("Remove Bo"), never "OK". */
  confirm: string;
  /** Coral confirm for removals and revocations; sand for granting power. */
  tone?: "primary" | "destructive";
  busy?: boolean;
  /** Shown above the buttons (role=alert): why the action failed, in the dialog. */
  error?: string | null;
  onConfirm: () => void;
  finalFocus?: RefObject<HTMLElement | null>;
}

/** A confirmation for an action that takes power away or hands it out: `role=alertdialog`. */
export function ConfirmDialog({ open, onOpenChange, title, body, confirm, tone = "primary", busy = false, error, onConfirm, finalFocus }: ConfirmDialogProps) {
  return (
    <AlertDialog.Root open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
      <AlertDialog.Portal>
        <AlertDialog.Backdrop className={backdrop} />
        <AlertDialog.Popup finalFocus={finalFocus} className={cn(popupFrame, popupSurface, "w-[28rem] space-y-4")}>
          <AlertDialog.Title className="text-lg font-semibold text-balance">{title}</AlertDialog.Title>
          <AlertDialog.Description className="text-sm text-chalk">{body}</AlertDialog.Description>
          {error && (
            <p role="alert" className="text-sm text-coral">
              {error}
            </p>
          )}
          <div className="flex justify-end gap-2">
            <AlertDialog.Close render={<Button variant="ghost" disabled={busy} />}>Cancel</AlertDialog.Close>
            <Button variant={tone === "destructive" ? "destructive" : "default"} disabled={busy} onClick={onConfirm}>
              {busy ? "Working…" : confirm}
            </Button>
          </div>
        </AlertDialog.Popup>
      </AlertDialog.Portal>
    </AlertDialog.Root>
  );
}
