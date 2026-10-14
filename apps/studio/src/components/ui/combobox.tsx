import { Autocomplete } from "@base-ui/react/autocomplete";
import { Combobox } from "@base-ui/react/combobox";
import { X } from "lucide-react";
import { useId, useState, type ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface ComboOption {
  value: string;
  /** A short note after the label in chalk-dim ("also gives billing", "only on orders"). */
  detail?: string;
  /** Why it can't be chosen now ("Admin only: LOGIN role"); the option shows but is disabled. */
  disabledReason?: string;
}

export interface MultiComboboxProps {
  label: string;
  options: readonly ComboOption[];
  value: string[];
  onValueChange: (value: string[]) => void;
  /** What each chosen chip adds after its name ("+ billing"). */
  chipDetail?: (value: string) => string | undefined;
  /** Chips drawn coral-outlined (refused by the server, or not allowed at the chosen level). */
  invalid?: ReadonlySet<string>;
  placeholder?: string;
  disabled?: boolean;
  /** Shown in the popup instead of options (loading, an error, an empty source). */
  status?: ReactNode;
  /** Lines shown at the top of the popup (sources left out, and why). */
  notes?: readonly string[];
  /** Mono labels: the values are code (role names). */
  mono?: boolean;
  /**
   * Where the popup opens (default below). A field above a dialog's buttons opens it above, so
   * the open list never covers them (it flips when there's no room).
   */
  side?: "top" | "bottom";
}

const optionRow =
  "grid cursor-default grid-cols-[1rem_1fr] items-start gap-2 px-2 py-1.5 outline-none select-none data-[highlighted]:bg-chalk/[0.08] data-[highlighted]:outline-solid data-[highlighted]:outline-2 data-[highlighted]:-outline-offset-2 data-[highlighted]:outline-sand data-[disabled]:text-chalk-dim";

/**
 * A multi-select combobox: a field that filters, chips for what's chosen and a listbox popup
 * (deep water, strong hairline, square). Arrow keys move, Enter toggles, Backspace in an empty
 * field removes the last chip, Escape closes. Disabled options stay listed with their reason.
 */
export function MultiCombobox({ label, options, value, onValueChange, chipDetail, invalid, placeholder, disabled, status, notes, mono, side = "bottom" }: MultiComboboxProps) {
  const id = useId();
  const byValue = new Map(options.map((o) => [o.value, o]));
  // Enabled options first, then the ones that can't be chosen (with their reasons).
  const ordered = [...options].sort((a, b) => Number(!!a.disabledReason) - Number(!!b.disabledReason));
  const code = mono ? "font-mono text-[13px]" : undefined;
  return (
    <Combobox.Root
      items={status ? [] : ordered.map((o) => o.value)}
      multiple
      value={value}
      onValueChange={(next) => onValueChange(next as string[])}
      disabled={disabled}
    >
      <div className="space-y-1">
        <label htmlFor={id} className="text-sm font-medium">
          {label}
        </label>
        <Combobox.InputGroup className="flex min-h-8 w-full cursor-text flex-wrap items-center gap-1 border border-input bg-ink px-1.5 py-1 focus-within:outline-2 focus-within:-outline-offset-1 focus-within:outline-sand">
          <Combobox.Chips className="flex w-full flex-wrap items-center gap-1" aria-label={value.length > 0 ? `Chosen ${label.toLowerCase()}` : undefined}>
            {value.map((v) => {
              const extra = chipDetail?.(v);
              const bad = invalid?.has(v) ?? false;
              return (
                <Combobox.Chip
                  key={v}
                  aria-label={v}
                  className={cn(
                    "flex items-center gap-1 border px-1.5 text-sm leading-6 outline-none focus-within:outline-2 focus-within:outline-sand",
                    bad ? "border-coral text-coral" : "border-line-strong text-chalk",
                  )}
                >
                  <span className={code}>{v}</span>
                  {extra && <span className="text-chalk-dim">{extra}</span>}
                  <Combobox.ChipRemove aria-label={`Remove ${v}`} className="flex size-4 items-center justify-center hover:bg-chalk/[0.08]">
                    <X aria-hidden="true" size={12} strokeWidth={1.5} />
                  </Combobox.ChipRemove>
                </Combobox.Chip>
              );
            })}
            <Combobox.Input
              id={id}
              placeholder={value.length > 0 ? "" : placeholder}
              className="h-6 min-w-24 flex-1 border-0 bg-transparent px-1 text-sm text-chalk outline-none placeholder:text-chalk-dim"
            />
          </Combobox.Chips>
        </Combobox.InputGroup>
      </div>
      <Combobox.Portal>
        <Combobox.Positioner side={side} sideOffset={4} className="z-[60] outline-none">
          <Combobox.Popup className="max-h-[min(var(--available-height),20rem)] w-[var(--anchor-width)] overflow-y-auto border border-line-strong bg-deep p-1 text-sm text-chalk">
            {notes?.map((n) => (
              <p key={n} className="px-2 py-1.5 text-xs text-chalk-dim">
                {n}
              </p>
            ))}
            {/* The status is the list's empty state (no items while it shows), so the popup keeps
                a List: without one, Escape fell through and closed the dialog around it. */}
            <Combobox.Empty>
              <div className="px-2 py-1.5 text-chalk-dim">{status ?? "Nothing matches."}</div>
            </Combobox.Empty>
            <Combobox.List>
              {(v: string) => {
                const o = byValue.get(v)!;
                return (
                  <Combobox.Item key={v} value={v} disabled={!!o.disabledReason} className={optionRow}>
                    <Combobox.ItemIndicator className="col-start-1 mt-1 size-2 bg-sand" />
                    <span className="col-start-2">
                      <span className={code}>{v}</span>
                      {o.detail && <span className="text-chalk-dim"> {o.detail}</span>}
                      {o.disabledReason && <span className="block text-xs text-chalk-dim">{o.disabledReason}</span>}
                    </span>
                  </Combobox.Item>
                );
              }}
            </Combobox.List>
          </Combobox.Popup>
        </Combobox.Positioner>
      </Combobox.Portal>
    </Combobox.Root>
  );
}

export interface FreeTextOption {
  value: string;
  /** A short note after the value in chalk-dim (a person's name). */
  detail?: string;
}

export interface FreeTextComboboxProps {
  label: string;
  /** Suggestions; entries with an empty value are dropped. */
  options: readonly FreeTextOption[];
  value: string;
  onValueChange: (value: string) => void;
  type?: "text" | "email";
  invalid?: boolean;
  describedBy?: string;
  disabled?: boolean;
  required?: boolean;
}

/**
 * A single-value combobox that accepts any text: suggestions filter as you type and picking one
 * fills the field, but nothing forces a choice (an email that isn't listed is just typed).
 * Shares the popup look of {@link MultiCombobox}.
 */
export function FreeTextCombobox({ label, options, value, onValueChange, type = "text", invalid, describedBy, disabled, required }: FreeTextComboboxProps) {
  const id = useId();
  const usable = options.filter((o) => o.value.trim() !== "");
  const byValue = new Map(usable.map((o) => [o.value, o]));
  // The popup opens only while something matches what's typed: an empty popup would still hold the dialog's focus.
  const [wantsOpen, setWantsOpen] = useState(false);
  const needle = value.trim().toLowerCase();
  const matches = usable.some((o) => o.value.toLowerCase().includes(needle) && o.value.toLowerCase() !== needle);
  return (
    <Autocomplete.Root
      items={usable.map((o) => o.value)}
      value={value}
      onValueChange={(next) => onValueChange(next)}
      open={wantsOpen && matches}
      onOpenChange={setWantsOpen}
      disabled={disabled}
    >
      <div className="space-y-1">
        <label htmlFor={id} className="text-sm font-medium">
          {label}
        </label>
        <Autocomplete.Input
          id={id}
          type={type}
          required={required}
          autoComplete="off"
          aria-invalid={invalid ? true : undefined}
          aria-describedby={describedBy}
          className="h-8 w-full border border-input bg-ink px-2 text-sm text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand aria-[invalid=true]:border-coral"
        />
      </div>
      <Autocomplete.Portal>
        <Autocomplete.Positioner sideOffset={4} className="z-[60] outline-none">
          <Autocomplete.Popup className="max-h-[min(var(--available-height),20rem)] w-[var(--anchor-width)] overflow-y-auto border border-line-strong bg-deep p-1 text-sm text-chalk">
            <Autocomplete.List>
              {(v: string) => {
                const o = byValue.get(v);
                return (
                  <Autocomplete.Item key={v} value={v} className={optionRow}>
                    <span className="col-start-2">
                      {v}
                      {o?.detail && <span className="text-chalk-dim"> {o.detail}</span>}
                    </span>
                  </Autocomplete.Item>
                );
              }}
            </Autocomplete.List>
          </Autocomplete.Popup>
        </Autocomplete.Positioner>
      </Autocomplete.Portal>
    </Autocomplete.Root>
  );
}
