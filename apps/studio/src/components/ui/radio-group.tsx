import { Radio } from "@base-ui/react/radio";
import { RadioGroup as RadioGroupPrimitive } from "@base-ui/react/radio-group";
import { useId } from "react";
import { cn } from "@/lib/utils";

export interface RadioOption<T extends string> {
  value: T;
  label: string;
  /** One line under the label (the option's meaning). */
  hint?: string;
  /** Shown but not choosable (the caller says why next to the group). */
  disabled?: boolean;
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
}

const focus = "outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand";

/**
 * A labelled radio group. Each option is one `role=radio` element holding its text, so the whole
 * option is the target. Arrow keys move the choice; the chosen option carries sand.
 */
export function RadioGroup<T extends string>({ label, options, value, onValueChange, disabled, layout = "stacked" }: RadioGroupProps<T>) {
  const labelId = useId();
  const segmented = layout === "segmented";
  return (
    <div className="space-y-1.5">
      <div id={labelId} className="text-sm font-medium">
        {label}
      </div>
      <RadioGroupPrimitive
        aria-labelledby={labelId}
        value={value}
        onValueChange={(next) => onValueChange(next as T)}
        disabled={disabled}
        className={segmented ? "inline-flex border border-input" : "flex flex-col gap-2"}
      >
        {options.map((o) =>
          segmented ? (
            <Radio.Root
              key={o.value}
              value={o.value}
              disabled={o.disabled}
              className={cn(
                focus,
                "cursor-default border-r border-input px-3 py-1.5 text-sm font-medium text-chalk last:border-r-0",
                "hover:bg-chalk/[0.06] data-[checked]:bg-sand data-[checked]:text-ink data-[disabled]:opacity-50",
              )}
            >
              {o.label}
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
                {o.hint && <span className="block text-chalk-dim">{o.hint}</span>}
              </span>
            </Radio.Root>
          ),
        )}
      </RadioGroupPrimitive>
    </div>
  );
}
