import { Tabs as TabsPrimitive } from "@base-ui/react/tabs";
import { useEffect, type ReactNode } from "react";
import { useScrollFade } from "@/lib/use-scroll-fade";

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
  const scroller = useScrollFade<HTMLDivElement>();
  // A strip that scrolls (a phone) keeps the current tab in view.
  useEffect(() => {
    scroller.ref.current?.querySelector<HTMLElement>("[data-active]")?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value]);
  return (
    <TabsPrimitive.Root value={value} onValueChange={(next) => onValueChange(next as T)} className="space-y-6">
      <div ref={scroller.ref} onScroll={scroller.onScroll} style={scroller.style} className="overflow-x-auto">
      <TabsPrimitive.List aria-label={label} className="flex w-max min-w-full gap-1 border-b border-line">
        {items.map((item) => (
          <TabsPrimitive.Tab
            key={item.id}
            value={item.id}
            className="shrink-0 border-b-2 whitespace-nowrap border-transparent px-3 py-2 text-sm font-medium text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand data-[active]:border-sand data-[active]:text-chalk"
          >
            {item.label}
          </TabsPrimitive.Tab>
        ))}
      </TabsPrimitive.List>
      </div>
      <TabsPrimitive.Panel value={value} className="outline-none focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-sand">
        {children}
      </TabsPrimitive.Panel>
    </TabsPrimitive.Root>
  );
}
