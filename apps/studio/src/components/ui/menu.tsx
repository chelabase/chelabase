import { Menu as MenuPrimitive } from "@base-ui/react/menu";
import type { ReactElement, ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface MenuProps {
  /** The element that opens the menu (rendered as the trigger, e.g. an icon Button). */
  trigger: ReactElement;
  children: ReactNode;
  align?: "start" | "end";
  /** Where focus goes when the menu closes (the trigger by default): an element, or false to leave it. */
  finalFocus?: () => HTMLElement | false | null;
}

/** A menu styled as the account menu: deep water, strong hairline, square, highlighted items outlined in sand. */
export function Menu({ trigger, children, align = "end", finalFocus }: MenuProps) {
  return (
    <MenuPrimitive.Root>
      <MenuPrimitive.Trigger render={trigger} />
      <MenuPrimitive.Portal>
        <MenuPrimitive.Positioner align={align} sideOffset={6} className="z-50">
          <MenuPrimitive.Popup finalFocus={finalFocus ? () => finalFocus() ?? true : undefined} className="min-w-48 border border-line-strong bg-deep p-1 text-sm text-chalk outline-none">
            {children}
          </MenuPrimitive.Popup>
        </MenuPrimitive.Positioner>
      </MenuPrimitive.Portal>
    </MenuPrimitive.Root>
  );
}

export interface MenuItemProps {
  onClick: () => void;
  children: ReactNode;
  /** Coral text for actions that take something away. */
  destructive?: boolean;
  disabled?: boolean;
}

export function MenuItem({ onClick, children, destructive = false, disabled }: MenuItemProps) {
  return (
    <MenuPrimitive.Item
      disabled={disabled}
      onClick={onClick}
      className={cn(
        "flex cursor-default items-center gap-2 px-2 py-1.5 outline-none select-none data-[disabled]:opacity-50",
        "data-[highlighted]:bg-chalk/[0.08] data-[highlighted]:outline-solid data-[highlighted]:outline-2 data-[highlighted]:-outline-offset-2 data-[highlighted]:outline-sand",
        destructive && "text-coral",
      )}
    >
      {children}
    </MenuPrimitive.Item>
  );
}

/** A hairline between groups of items. */
export function MenuSeparator() {
  return <MenuPrimitive.Separator className="my-1 h-px bg-line" />;
}

/** A line of explanation inside a menu (not an item: never focused). */
export function MenuNote({ children }: { children: ReactNode }) {
  return <div className="px-2 py-1.5 text-xs text-chalk-dim">{children}</div>;
}
