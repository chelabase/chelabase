import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import type { ReactNode, RefObject } from "react";
import { cn } from "@/lib/utils";

export interface ModalProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  /** One line under the title (the dialog's accessible description). */
  description?: ReactNode;
  /** `wide` fits forms with rows of fields (the invite dialog); `medium` (38rem) holds a secret shown once. */
  size?: "default" | "medium" | "wide";
  /** Where focus goes on open (default: the first focusable) and on close (default: the trigger). */
  initialFocus?: RefObject<HTMLElement | null>;
  /** A function decides at close time; `false` leaves focus alone (another dialog takes it). */
  finalFocus?: RefObject<HTMLElement | null> | (() => HTMLElement | boolean | null);
  /** A click outside doesn't close it (dialogs holding something shown once). */
  disablePointerDismissal?: boolean;
  children: ReactNode;
}

/**
 * The popup surface, shared by every dialog: deep water, a strong hairline, square, no
 * shadow; the page behind dims. Focus is trapped and Escape closes it.
 */
export const popupSurface = "border border-line-strong bg-deep text-chalk";
export const backdrop = "fixed inset-0 z-40 bg-ink/70";
export const popupFrame =
  "fixed top-1/2 left-1/2 z-50 max-h-[calc(100vh-4rem)] max-w-[calc(100vw-2rem)] -translate-x-1/2 -translate-y-1/2 overflow-y-auto p-6 outline-none transition-opacity duration-150 data-[ending-style]:opacity-0 data-[starting-style]:opacity-0 motion-reduce:transition-none";

export function Modal({ open, onOpenChange, title, description, size = "default", initialFocus, finalFocus, disablePointerDismissal, children }: ModalProps) {
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange} disablePointerDismissal={disablePointerDismissal}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Backdrop className={backdrop} />
        <DialogPrimitive.Popup
          initialFocus={initialFocus}
          finalFocus={finalFocus}
          className={cn(popupFrame, popupSurface, "space-y-4", size === "wide" ? "w-[44rem]" : size === "medium" ? "w-[38rem]" : "w-[28rem]")}
        >
          <div className="space-y-1">
            <DialogPrimitive.Title className="text-lg font-semibold text-balance">{title}</DialogPrimitive.Title>
            {description && (
              <DialogPrimitive.Description className="text-sm text-chalk-dim">{description}</DialogPrimitive.Description>
            )}
          </div>
          {children}
        </DialogPrimitive.Popup>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
