import { Tooltip } from "@base-ui/react/tooltip";
import { Link } from "@tanstack/react-router";
import { cn } from "@/lib/utils";
import { isRailItemActive, type RailEntry } from "./rail-items";

export interface RailItemProps {
  entry: RailEntry;
  pathname: string;
  /** Labels show beside the icons; otherwise they stay in the DOM as screen-reader text. */
  expanded: boolean;
  /** Collapsed mode only: each link shows its label in a tooltip on hover or focus. */
  tooltip: boolean;
}

/** Row geometry shared by links and placeholders: the 18px icon sits centred in the 56px rail. */
const row = "relative flex h-9 items-center gap-3 whitespace-nowrap text-sm";

function Label({ text, expanded }: { text: string; expanded: boolean }) {
  return <span className={expanded ? "truncate" : "sr-only"}>{text}</span>;
}

/** One rail entry: a section link (with the sand edge-mark when current) or a not-built-yet placeholder. */
export function RailItem({ entry, pathname, expanded, tooltip }: RailItemProps) {
  const Icon = entry.icon;
  const icon = <Icon aria-hidden="true" size={18} strokeWidth={1.5} className="shrink-0" />;

  if (entry.kind === "placeholder") {
    return (
      <li>
        <div
          aria-disabled="true"
          className={cn(row, "mx-2 border border-dashed border-line-strong px-[10px] text-chalk-dim")}
        >
          {icon}
          <Label text={entry.label} expanded={expanded} />
        </div>
      </li>
    );
  }

  const active = isRailItemActive(entry.id, pathname);
  const link = (
    <Link
      to={entry.to}
      aria-current={active ? "page" : undefined}
      activeOptions={{ exact: true }}
      activeProps={{}}
      className={cn(
        row,
        "px-[19px] transition-colors hover:bg-chalk/[0.06] hover:text-chalk",
        "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand",
        active
          ? "text-chalk before:absolute before:inset-y-1.5 before:left-0 before:w-0.5 before:bg-sand"
          : "text-chalk-dim",
      )}
    >
      {icon}
      <Label text={entry.label} expanded={expanded} />
    </Link>
  );

  if (!tooltip) return <li>{link}</li>;

  return (
    <li>
      <Tooltip.Root>
        <Tooltip.Trigger render={link} />
        <Tooltip.Portal>
          <Tooltip.Positioner side="right" sideOffset={8} className="z-50">
            <Tooltip.Popup className="border border-line-strong bg-deep px-2 py-1 text-xs text-chalk">
              {entry.label}
            </Tooltip.Popup>
          </Tooltip.Positioner>
        </Tooltip.Portal>
      </Tooltip.Root>
    </li>
  );
}
