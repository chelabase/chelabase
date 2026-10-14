import { useEffect, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";

const SLOT_ID = "app-header-context";

/**
 * Renders its children into the shell header's context slot (`#app-header-context`). Without the
 * slot (tests, other layouts) they render in place. Until its effect has looked for the slot, it
 * renders nothing (unless the slot is already there): on a cold load the header may mount after
 * this component, and rendering in place first would mount the children twice and flash.
 */
export function HeaderContext({ children }: { children: ReactNode }) {
  const [state, setState] = useState<{ slot: HTMLElement | null; looked: boolean }>(() => {
    const slot = typeof document === "undefined" ? null : document.getElementById(SLOT_ID);
    return { slot, looked: slot !== null };
  });
  useEffect(() => {
    // The header may mount after this component does.
    setState({ slot: document.getElementById(SLOT_ID), looked: true });
  }, []);
  if (state.slot) return createPortal(children, state.slot);
  return state.looked ? <>{children}</> : null;
}
