import { Combobox } from "@base-ui/react/combobox";
import { useState } from "react";
import { ChevronDown } from "lucide-react";
import { cn } from "@/lib/utils";

export interface PickerOption {
  value: string;
  /** What the field shows when this is the current value. */
  label: string;
  /** Tree depth: indents the row. */
  depth?: number;
  /** Chalk-dim text at the row's end ("2 hours ago"). */
  note?: string;
  /** A small outlined tag after the label ("protected", "data API"). */
  tag?: string;
  /** A command row, not a value: always listed (even when filtering) and never becomes the field's value. */
  action?: boolean;
}

export interface PickerProps {
  /** Accessible name of the field, and the small caption in the `header` look. */
  label: string;
  options: readonly PickerOption[];
  value: string | null;
  onChange: (value: string) => void;
  /** Mono labels: the values are identifiers. */
  mono?: boolean;
  /** `header`: caption and value in one bordered box; `field`: the value alone, filling its parent. */
  look?: "header" | "field";
  disabled?: boolean;
  placeholder?: string;
  /** The id of the text that describes the field (its error). */
  describedBy?: string;
  /**
   * Typed values: when the typed text matches no option and this accepts it, the list offers
   * `Use "<text>"`, which makes the text the value.
   */
  acceptTyped?: (text: string) => boolean;
  /** The `field` look with its label as a small chalk-dim caption before the value (the table list's schema). */
  caption?: boolean;
  /** Called when the list opens or closes. */
  onOpenChange?: (open: boolean) => void;
}

/** The value of the `Use "<text>"` row (never a real option's value: those don't start with NUL). */
const TYPED = "\u0000typed:";

const row =
  "flex cursor-default items-center justify-between gap-4 px-3 py-2 outline-none select-none data-[highlighted]:bg-chalk/[0.08] data-[highlighted]:outline-solid data-[highlighted]:outline-2 data-[highlighted]:-outline-offset-2 data-[highlighted]:outline-sand";

/**
 * A searchable single-choice menu: a Base UI combobox that opens on a click in its field, shows
 * the current choice as its value and lists one level's options in a deep-water popup.
 */
/** Header picker value sizing: its text width, never forced wider than it needs (min of 7rem and its own width). */
export function headerWidth(shown: string): { flex: string; width: number; minWidth: string } {
  const n = Math.min(28, Math.max(7, shown.length + 1));
  // `width: 0` + a `Nch` basis: the field's min-content is its minimum, so the bar's pickers shrink together.
  return { flex: `1 1 ${n}ch`, width: 0, minWidth: `min(7rem, ${n}ch)` };
}

export function Picker({ label, options, value, onChange, mono, look = "header", disabled, placeholder, describedBy, acceptTyped, caption, onOpenChange }: PickerProps) {
  const [typed, setTyped] = useState("");
  const text = typed.trim();
  const offerTyped = !!acceptTyped && text !== "" && text !== value && !options.some((o) => o.value === text || o.label === text) && acceptTyped(text);
  const all: readonly PickerOption[] = offerTyped ? [...options, { value: `${TYPED}${text}`, label: `Use "${text}"`, action: true }] : options;
  const byValue = new Map(all.map((o) => [o.value, o]));
  const current = value === null ? undefined : byValue.get(value);
  const shown = current?.label ?? "";
  const code = mono ? "font-mono text-[13px]" : undefined;
  return (
    <Combobox.Root<string>
      items={all.map((o) => o.value)}
      value={value}
      onValueChange={(next) => {
        if (next === null) return;
        onChange(next.startsWith(TYPED) ? next.slice(TYPED.length) : next);
      }}
      onInputValueChange={acceptTyped ? (v) => setTyped(v) : undefined}
      itemToStringLabel={(v) => byValue.get(v)?.label ?? v}
      filter={(item, query, toString) => {
        const o = byValue.get(item);
        if (o?.action) return true;
        return (toString?.(item) ?? item).toLowerCase().includes(query.trim().toLowerCase());
      }}
      openOnInputClick
      onOpenChange={onOpenChange ? (open) => onOpenChange(open) : undefined}
      disabled={disabled}
    >
      <div
        className={cn(
          "relative flex items-center gap-2 border border-line-strong focus-within:outline-2 focus-within:-outline-offset-1 focus-within:outline-sand",
          look === "header" ? "h-9 min-w-0 pr-6 pl-2" : "h-9 w-full pr-6 pl-2",
          disabled && "opacity-60",
        )}
      >
        {look === "header" && (
          // The context bar's captions give way below `sm`; the field keeps its accessible name.
          <span aria-hidden="true" className="hidden shrink-0 text-[11px] text-chalk-dim sm:inline">
            {label}
          </span>
        )}
        {look === "field" && caption && (
          <span aria-hidden="true" className="shrink-0 text-xs text-chalk-dim">
            {label}
          </span>
        )}
        <Combobox.Input
          aria-label={label}
          aria-describedby={describedBy}
          title={shown || undefined}
          placeholder={placeholder}
          className={cn(
            "h-full min-w-0 border-0 bg-transparent p-0 text-sm text-chalk outline-none placeholder:text-chalk-dim",
            look === "field" ? "w-full" : "max-w-[16rem] truncate",
            code,
          )}
          style={look === "header" ? { ...headerWidth(shown) } : undefined}
        />
        <ChevronDown aria-hidden="true" size={14} strokeWidth={1.5} className="pointer-events-none absolute right-2 text-chalk-dim" />
      </div>
      <Combobox.Portal>
        <Combobox.Positioner sideOffset={4} align="start" className="z-[60] outline-none">
          <Combobox.Popup className="max-h-[min(var(--available-height),22rem)] min-w-[max(var(--anchor-width),14rem)] max-w-[28rem] overflow-y-auto border border-line-strong bg-deep py-1 text-sm text-chalk">
            <Combobox.Empty>
              <div className="px-3 py-2 text-chalk-dim">Nothing matches.</div>
            </Combobox.Empty>
            <Combobox.List>
              {(v: string) => {
                const o = byValue.get(v);
                if (!o) return null;
                return (
                  <Combobox.Item
                    key={v}
                    value={v}
                    className={cn(row, o.action && "mt-1 border-t border-line text-chalk-dim")}
                    style={o.depth ? { paddingLeft: `${12 + o.depth * 14}px` } : undefined}
                  >
                    <span className="flex min-w-0 items-center gap-2">
                      {v === value && !o.action && <span aria-hidden="true" className="size-1.5 shrink-0 bg-sand" />}
                      <span className={cn("truncate", code)}>{o.label}</span>
                      {o.tag && <span className="border border-line px-1 text-[11px] text-chalk-dim">{o.tag}</span>}
                    </span>
                    {o.note && <span className="shrink-0 text-xs text-chalk-dim">{o.note}</span>}
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
