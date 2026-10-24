import { Radio } from "@base-ui/react/radio";
import { RadioGroup as RadioGroupPrimitive } from "@base-ui/react/radio-group";
import { useId, type ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface RadioOption<T extends string> {
  value: T;
  label: string;
  /** One line under the label (the option's meaning). */
  hint?: string;
  /** Shown but not choosable (the caller says why next to the group). */
  disabled?: boolean;
  /** A small mark before a segmented option's label. */
  icon?: ReactNode;
}

export interface RadioGroupProps<T extends string> {
  /** The group's visible label. */
  label: string;
  options: readonly RadioOption<T>[];
  value: T;
  onValueChange: (value: T) => void;
  disabled?: boolean;
  /** `segmented`: one row of joined options (platform roles); `stacked`: one per line with hints. */
  layout?: "segmented" | "stacked";
  /** A segmented group's chosen option: `sand` (filled, the default) or `tab` (channel with a sand underline, like the Rows | Structure tabs). */
  tone?: "sand" | "tab";
  /** Names the group for assistive technology only (a toolbar control whose options say it). */
  hideLabel?: boolean;
  /** `stacked` only: the options sit in one wrapping row (hints follow their label) instead of one per line. */
  inline?: boolean;
  /** `segmented` only: the group takes the full width in `cols` equal columns below sm (it never wraps a label). */
  cols?: number;
  /** Extra classes for the visible label (e.g. `sm:sr-only`). */
  labelClassName?: string;
}

const focus = "outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand";

/**
 * A labelled radio group. Each option is one `role=radio` element holding its text, so the whole
 * option is the target. Arrow keys move the choice; the chosen option carries sand.
 */
export function RadioGroup<T extends string>({ label, options, value, onValueChange, disabled, layout = "stacked", tone = "sand", hideLabel, inline, cols, labelClassName }: RadioGroupProps<T>) {
  const labelId = useId();
  const segmented = layout === "segmented";
  return (
    <div className="space-y-1.5">
      <div id={labelId} className={hideLabel ? "sr-only" : cn("text-sm font-medium", labelClassName)}>
        {label}
      </div>
      <RadioGroupPrimitive
        aria-labelledby={labelId}
        value={value}
        onValueChange={(next) => onValueChange(next as T)}
        disabled={disabled}
        className={
          segmented
            ? cn("border border-input", cols === 4 ? "grid w-full grid-cols-2 sm:inline-flex sm:w-auto" : cols === 3 ? "grid w-full grid-cols-3 sm:inline-flex sm:w-auto" : "inline-flex")
            : inline
              ? "flex flex-row flex-wrap gap-x-6 gap-y-2"
              : "flex flex-col gap-2"
        }
      >
        {options.map((o) =>
          segmented ? (
            <Radio.Root
              key={o.value}
              value={o.value}
              disabled={o.disabled}
              className={cn(
                focus,
                "cursor-default whitespace-nowrap border-r border-input px-3 py-1.5 text-sm font-medium text-chalk last:border-r-0",
                "hover:bg-chalk/[0.06] data-[disabled]:opacity-50",
                tone === "tab"
                  ? "text-chalk-dim hover:text-chalk data-[checked]:bg-channel data-[checked]:text-chalk data-[checked]:shadow-[inset_0_-2px_0_var(--sand)]"
                  : "data-[checked]:bg-sand data-[checked]:text-ink",
              )}
            >
              {o.icon ? (
                <span className="inline-flex items-center gap-1.5">
                  {o.icon}
                  {o.label}
                </span>
              ) : (
                o.label
              )}
            </Radio.Root>
          ) : (
            <Radio.Root
              key={o.value}
              value={o.value}
              disabled={o.disabled}
              className={cn(focus, "group flex cursor-default items-start gap-2 text-left text-sm data-[disabled]:opacity-50")}
            >
              {/* Round, unlike the square checkboxes: one of these, not any of them. */}
              <span className="mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-full border border-input group-data-[checked]:border-sand">
                <Radio.Indicator className="size-2 rounded-full bg-sand" />
              </span>
              <span>
                <span className="text-chalk">{o.label}</span>
                {o.hint && <span className={inline ? "ml-2 text-chalk-dim" : "block text-chalk-dim"}>{o.hint}</span>}
              </span>
            </Radio.Root>
          ),
        )}
      </RadioGroupPrimitive>
    </div>
  );
}
