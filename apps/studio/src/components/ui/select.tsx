import type { ComponentProps } from "react";
import { ChevronDown } from "lucide-react";
import { cn } from "@/lib/utils";

/** The look of a select field; callers may override size and spacing through `className`. */
export const selectClass =
  "h-8 w-full min-w-0 appearance-none rounded-none border border-input bg-ink pr-7 pl-2 text-sm text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand disabled:opacity-60 aria-[invalid=true]:border-coral";

/**
 * A native `<select>` without the OS look: `appearance-none` with a drawn chevron in the same
 * wrapper. Labels, keys and `selectOptions` work as on any select.
 */
export function Select({ className, wrapperClassName, children, ref, ...props }: ComponentProps<"select"> & { wrapperClassName?: string }) {
  return (
    <span className={cn("relative flex min-w-0", wrapperClassName)}>
      <select ref={ref} {...props} className={cn(selectClass, className)}>
        {children}
      </select>
      <ChevronDown aria-hidden="true" size={14} strokeWidth={1.5} className="pointer-events-none absolute top-1/2 right-2 -translate-y-1/2 text-chalk-dim" />
    </span>
  );
}
