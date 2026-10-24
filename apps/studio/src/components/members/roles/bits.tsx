import { useEffect, useRef, type ReactNode } from "react";
import { Checkbox } from "@/components/table-editor/panel-bits";
import { cn } from "@/lib/utils";

/** A square tag: micro text in a hairline frame; `hazard` is coral (owner-level). */
export function Tag({ children, hazard }: { children: ReactNode; hazard?: boolean }) {
  return (
    <span className={cn("inline-flex items-center whitespace-nowrap border px-1.5 py-px text-[11px] leading-4", hazard ? "border-coral/60 text-coral" : "border-line-strong text-chalk-dim")}>
      {children}
    </span>
  );
}

/**
 * The editor's square checkbox with a mixed state (`checked: "mixed"`, a sand bar). Native, so a
 * disabled one is really disabled (`disabled`), and labels and keys work as usual.
 */
export function TriCheckbox({
  checked,
  onChange,
  disabled,
  label,
  className,
}: {
  checked: boolean | "mixed";
  onChange: () => void;
  disabled?: boolean;
  label: string;
  className?: string;
}) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = checked === "mixed";
  }, [checked]);
  return <Checkbox ref={ref} aria-label={label} checked={checked === true} disabled={disabled} onChange={onChange} className={className} />;
}

/**
 * An on/off switch as a native button (`role="switch"`), so a read-only one carries `disabled`:
 * the square hairline track and thumb of `ui/switch`, seafoam when on, a sand edge when staged.
 */
export function RoleSwitch({ checked, onChange, disabled, label, staged }: { checked: boolean; onChange: () => void; disabled?: boolean; label: string; staged?: boolean }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={onChange}
      data-checked={checked ? "" : undefined}
      className={cn(
        "group relative inline-flex h-6 w-11 shrink-0 items-center border border-input bg-ink p-0.5 transition-colors duration-150 outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand disabled:opacity-50 motion-reduce:transition-none",
        checked && "border-seafoam bg-seafoam/25",
        staged && "shadow-[inset_0_2px_0_rgb(242_199_163/0.45)]",
      )}
    >
      <span
        aria-hidden="true"
        className={cn("block size-4 bg-chalk-dim transition-transform duration-150 ease-lagoon motion-reduce:transition-none", checked && "translate-x-5 bg-seafoam")}
      />
    </button>
  );
}

/** A dry-run line's leading square: seafoam (passed), outlined coral (warning), solid coral (error). */
export function Square({ tone }: { tone: "ok" | "warn" | "error" }) {
  return <span aria-hidden="true" className={cn("mt-[5px] size-2 shrink-0", tone === "ok" ? "bg-seafoam" : tone === "error" ? "bg-coral" : "border border-coral")} />;
}

/** A staged cell's mark: the sand wash and the 2px sand-45% top edge. */
export const stagedCell = "bg-sand/[0.08] shadow-[inset_0_2px_0_rgb(242_199_163/0.45)]";
