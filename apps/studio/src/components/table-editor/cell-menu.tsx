import { Fragment } from "react";
import { ContextMenu } from "@base-ui/react/context-menu";
import { MenuItem } from "@/components/ui/menu";

/** One entry of the body-cell context menu. */
export interface CellMenuItem {
  label: string;
  onSelect: () => void;
  /** Coral text for actions that take something away. */
  destructive?: boolean;
  /** The action moves focus itself (an editor, a dialog): the menu doesn't return focus to the cell. */
  takesFocus?: boolean;
}

/** The menu's groups, in order; an empty group is dropped, and separators sit only between the groups shown. */
export type CellMenuGroups = CellMenuItem[][];

export interface CellMenuPopupProps {
  groups: CellMenuGroups;
  /** Where focus goes once the menu closes (false: where the user put it). */
  finalFocus: () => HTMLElement | false;
  /** An item was chosen, before the menu closes. */
  onPick: (item: CellMenuItem) => void;
}

/**
 * The popup of the grid's body-cell context menu (the Root and Trigger live in the grid). Keys pressed in it
 * never reach the grid: the popup is a React child of the grid, and synthetic events bubble through portals.
 */
export function CellMenuPopup({ groups, finalFocus, onPick }: CellMenuPopupProps) {
  const shown = groups.filter((g) => g.length > 0);
  return (
    <ContextMenu.Portal>
      <ContextMenu.Positioner className="z-50">
        <ContextMenu.Popup
          data-cell-menu=""
          finalFocus={finalFocus}
          onKeyDown={(e) => e.stopPropagation()}
          className="min-w-48 border border-line-strong bg-deep p-1 font-sans text-sm text-chalk outline-none"
        >
          {shown.map((group, i) => (
            <Fragment key={i}>
              {i > 0 && <ContextMenu.Separator className="my-1 h-px bg-line" />}
              {group.map((item) => (
                <MenuItem key={item.label} destructive={item.destructive} onClick={() => onPick(item)}>
                  {item.label}
                </MenuItem>
              ))}
            </Fragment>
          ))}
        </ContextMenu.Popup>
      </ContextMenu.Positioner>
    </ContextMenu.Portal>
  );
}
