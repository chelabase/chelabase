import { Menu } from "@base-ui/react/menu";
import { Tooltip } from "@base-ui/react/tooltip";
import { Check, PanelLeft } from "lucide-react";
import { useState } from "react";
import type { RailBehaviour } from "@/lib/rail-preference";

export interface RailBehaviourMenuProps {
  behaviour: RailBehaviour;
  onBehaviourChange: (next: RailBehaviour) => void;
  /** Reports the menu opening and closing; the rail stays expanded while it is open. */
  onOpenChange?: (open: boolean) => void;
  /** Icon-only trigger when false (its accessible name stays "Sidebar behaviour"). */
  labelsVisible: boolean;
}

const OPTIONS: readonly { value: RailBehaviour; label: string }[] = [
  { value: "expanded", label: "Expanded" },
  { value: "collapsed", label: "Collapsed" },
  { value: "hover", label: "Expand on hover" },
];

/** The rail's foot: picks whether the rail is pinned open, pinned shut, or opens on hover. */
export function RailBehaviourMenu({ behaviour, onBehaviourChange, onOpenChange, labelsVisible }: RailBehaviourMenuProps) {
  const [menuOpen, setMenuOpen] = useState(false);
  const trigger = (
    <Menu.Trigger
      aria-label="Sidebar behaviour"
      className="flex h-9 w-full items-center gap-3 overflow-hidden whitespace-nowrap px-[19px] text-sm text-chalk-dim transition-colors hover:bg-chalk/[0.06] hover:text-chalk focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand data-[popup-open]:bg-chalk/[0.06] data-[popup-open]:text-chalk"
    >
      <PanelLeft aria-hidden="true" size={18} strokeWidth={1.5} className="shrink-0" />
      {labelsVisible && <span>Sidebar</span>}
    </Menu.Trigger>
  );
  return (
    <Menu.Root
      onOpenChange={(open) => {
        setMenuOpen(open);
        onOpenChange?.(open);
      }}
    >
      {/* The wrapper stays mounted either way (the trigger must not remount when the rail widens
          with the menu open); the tooltip is off while the label shows or the menu is open. */}
      <Tooltip.Provider delay={300}>
        <Tooltip.Root disabled={labelsVisible || menuOpen}>
          <Tooltip.Trigger render={trigger} />
          <Tooltip.Portal>
            <Tooltip.Positioner side="right" sideOffset={8} className="z-50">
              <Tooltip.Popup className="border border-line-strong bg-deep px-2 py-1 text-xs text-chalk">
                Sidebar behaviour
              </Tooltip.Popup>
            </Tooltip.Positioner>
          </Tooltip.Portal>
        </Tooltip.Root>
      </Tooltip.Provider>
      <Menu.Portal>
        <Menu.Positioner side="right" align="end" sideOffset={8} className="z-50">
          <Menu.Popup className="min-w-52 border border-line-strong bg-deep py-1 text-sm text-chalk outline-none">
            <Menu.RadioGroup value={behaviour} onValueChange={(value) => onBehaviourChange(value as RailBehaviour)}>
              {OPTIONS.map((option) => (
                <Menu.RadioItem
                  key={option.value}
                  value={option.value}
                  closeOnClick
                  className="grid cursor-default grid-cols-[1rem_1fr] items-center gap-2 px-3 py-2 outline-none select-none data-[highlighted]:bg-chalk/[0.08] data-[highlighted]:outline-solid data-[highlighted]:outline-2 data-[highlighted]:-outline-offset-2 data-[highlighted]:outline-sand"
                >
                  <Menu.RadioItemIndicator className="col-start-1 flex text-sand">
                    <Check aria-hidden="true" size={16} strokeWidth={1.5} />
                  </Menu.RadioItemIndicator>
                  <span className="col-start-2">{option.label}</span>
                </Menu.RadioItem>
              ))}
            </Menu.RadioGroup>
          </Menu.Popup>
        </Menu.Positioner>
      </Menu.Portal>
    </Menu.Root>
  );
}
