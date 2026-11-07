import { Tooltip } from "@base-ui/react/tooltip";
import type { ReactElement } from "react";

/**
 * A disabled control with a tooltip saying why. Disabled controls take no pointer or focus, so the
 * tooltip hangs on a focusable wrapper. Without a reason the control is shown as is.
 */
export function ReadOnlyHint({ reason, children }: { reason: string | null; children: ReactElement }) {
  if (!reason) return children;
  return (
    <Tooltip.Root>
      <Tooltip.Trigger delay={150} render={<span tabIndex={0} className="inline-flex w-fit outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand" />}>
        {children}
      </Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Positioner side="top" sideOffset={6} className="z-50">
          <Tooltip.Popup role="tooltip" className="max-w-72 border border-line-strong bg-deep px-2 py-1 text-xs text-chalk">
            {reason}
          </Tooltip.Popup>
        </Tooltip.Positioner>
      </Tooltip.Portal>
    </Tooltip.Root>
  );
}

export const VIEWER_REASON = "Viewers can't change functions. Ask a Developer.";
export const VIEWER_SECRETS_REASON = "Viewers can't change secrets. Ask a Developer.";
