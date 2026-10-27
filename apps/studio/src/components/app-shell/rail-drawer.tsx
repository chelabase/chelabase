import { Dialog } from "@base-ui/react/dialog";
import { useRef, type ReactNode, type RefObject } from "react";

export interface RailDrawerProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The popup's id, for the menu button's `aria-controls`. */
  id: string;
  /** The header's menu button: focus returns here on close. */
  returnFocusRef: RefObject<HTMLElement | null>;
  /** The rail (`variant="drawer"`). */
  children: ReactNode;
}

/**
 * The rail as an overlay drawer below the narrow breakpoint: a modal Base UI dialog named "Menu"
 * (focus trap, Escape, backdrop click, scroll lock, inert page). Closing returns focus to the menu
 * button, or to `#content` when the button is gone (a resize to wide unmounts it).
 */
export function RailDrawer({ open, onOpenChange, id, returnFocusRef, children }: RailDrawerProps) {
  const popupRef = useRef<HTMLDivElement>(null);
  const returnTarget = () => {
    const button = returnFocusRef.current;
    if (button?.isConnected) return button;
    return document.getElementById("content");
  };
  return (
    <Dialog.Root
      open={open}
      onOpenChange={onOpenChange}
      onOpenChangeComplete={(isOpen) => {
        // A backdrop click leaves focus on the body (the scrim isn't focusable): put it back.
        if (!isOpen && (!document.activeElement || document.activeElement === document.body)) returnTarget()?.focus();
      }}
    >
      <Dialog.Portal>
        <Dialog.Backdrop className="fixed inset-0 z-40 bg-black/50 transition-opacity duration-200 data-[ending-style]:opacity-0 data-[starting-style]:opacity-0 motion-reduce:transition-none" />
        <Dialog.Popup
          id={id}
          ref={popupRef}
          initialFocus={() => popupRef.current?.querySelector<HTMLElement>("a[href]") ?? true}
          finalFocus={() => returnTarget() ?? true}
          className="fixed inset-y-0 left-0 z-50 w-[232px] max-w-[85vw] outline-none transition-transform duration-200 data-[ending-style]:-translate-x-full data-[starting-style]:-translate-x-full motion-reduce:transition-none"
        >
          <Dialog.Title className="sr-only">Menu</Dialog.Title>
          {children}
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
