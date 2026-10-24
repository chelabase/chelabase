import { Tabs as TabsPrimitive } from "@base-ui/react/tabs";
import type { ReactNode } from "react";

export interface TabItem<T extends string> {
  id: T;
  label: string;
}

export interface TabsProps<T extends string> {
  /** Names the tab list for screen readers ("Members sections"). */
  label: string;
  items: readonly TabItem<T>[];
  value: T;
  onValueChange: (value: T) => void;
  /** The current tab's content; it is rendered as that tab's panel. */
  children: ReactNode;
}

/**
 * A tab strip and the current panel. Arrow keys move between tabs, Enter or Space opens one; the
 * current tab is chalk with a 2px sand rule, the rest chalk-dim, all on one hairline.
 */
export function Tabs<T extends string>({ label, items, value, onValueChange, children }: TabsProps<T>) {
  return (
    <TabsPrimitive.Root value={value} onValueChange={(next) => onValueChange(next as T)} className="space-y-6">
      <TabsPrimitive.List aria-label={label} className="flex gap-1 overflow-x-auto border-b border-line">
        {items.map((item) => (
          <TabsPrimitive.Tab
            key={item.id}
            value={item.id}
            className="-mb-px shrink-0 border-b-2 whitespace-nowrap border-transparent px-3 py-2 text-sm font-medium text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand data-[active]:border-sand data-[active]:text-chalk"
          >
            {item.label}
          </TabsPrimitive.Tab>
        ))}
      </TabsPrimitive.List>
      <TabsPrimitive.Panel value={value} className="outline-none focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-sand">
        {children}
      </TabsPrimitive.Panel>
    </TabsPrimitive.Root>
  );
}
