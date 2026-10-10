import { Switch as SwitchPrimitive } from "@base-ui/react/switch";

export interface SwitchProps {
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  disabled?: boolean;
  /** The id the visible label points at (`<label htmlFor>`), or use `aria-label`. */
  id?: string;
  "aria-label"?: string;
  "aria-describedby"?: string;
}

/** An on/off switch: a square hairline track, a chalk thumb; on is seafoam (yes). Acts at once. */
export function Switch({ checked, onCheckedChange, disabled, id, ...aria }: SwitchProps) {
  return (
    <SwitchPrimitive.Root
      id={id}
      checked={checked}
      onCheckedChange={(next) => onCheckedChange(next)}
      disabled={disabled}
      {...aria}
      className="relative inline-flex h-6 w-11 shrink-0 items-center border border-input bg-ink p-0.5 transition-colors duration-150 outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand data-[checked]:border-seafoam data-[checked]:bg-seafoam/25 data-[disabled]:opacity-50 motion-reduce:transition-none"
    >
      <SwitchPrimitive.Thumb className="block size-4 bg-chalk-dim transition-transform duration-150 ease-lagoon data-[checked]:translate-x-5 data-[checked]:bg-seafoam motion-reduce:transition-none" />
    </SwitchPrimitive.Root>
  );
}
