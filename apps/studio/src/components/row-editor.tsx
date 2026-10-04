import { useState, type FormEvent } from "react";
import type { ColumnDesc } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { inputKind, parseValue } from "@/lib/console";

export interface FieldState {
  text: string;
  isNull: boolean;
  useDefault: boolean;
}

/** The text a value starts as in an input. */
export function toText(value: unknown, col?: ColumnDesc): string {
  if (value === null || value === undefined) return "";
  // A json cell is edited as JSON, so a string value keeps its quotes.
  if (col && inputKind(col) === "json") return JSON.stringify(value);
  return typeof value === "object" ? JSON.stringify(value) : String(value);
}

export function emptyField(col: ColumnDesc): FieldState {
  const kind = inputKind(col);
  return {
    text: kind === "bool" ? "false" : "",
    isNull: !col.has_default && col.nullable,
    useDefault: col.has_default,
  };
}

/** Turns a field into the value to send: `undefined` means "leave it out". */
export function fieldValue(col: ColumnDesc, f: FieldState): { ok: true; value: unknown } | { ok: false; error: string } {
  if (f.useDefault) return { ok: true, value: undefined };
  if (f.isNull) return { ok: true, value: null };
  const kind = inputKind(col);
  if (f.text.trim() === "" && kind !== "text") return { ok: false, error: "enter a value" };
  return parseValue(col, f.text);
}

export interface FieldInputProps {
  name: string;
  col: ColumnDesc;
  enumValues?: string[];
  state: FieldState;
  onChange: (next: FieldState) => void;
  /** Insert only: columns with a default can be left to it. */
  allowDefault?: boolean;
}

/** A type-aware input with its NULL (and, on insert, default) toggles. */
export function FieldInput({ name, col, enumValues, state, onChange, allowDefault }: FieldInputProps) {
  const kind = inputKind(col);
  const off = state.isNull || state.useDefault;
  const cls = "h-8 rounded-lg border border-input bg-background px-2 text-sm";
  const set = (text: string) => onChange({ ...state, text });
  let input;
  if (kind === "bool") {
    input = (
      <select aria-label={`${name} value`} value={state.text || "false"} disabled={off} onChange={(e) => set(e.target.value)} className={cls}>
        <option value="true">true</option>
        <option value="false">false</option>
      </select>
    );
  } else if (kind === "enum" && enumValues) {
    input = (
      <select aria-label={`${name} value`} value={state.text} disabled={off} onChange={(e) => set(e.target.value)} className={cls}>
        <option value="">Choose…</option>
        {enumValues.map((v) => (
          <option key={v}>{v}</option>
        ))}
      </select>
    );
  } else if (kind === "json") {
    input = (
      <textarea
        aria-label={`${name} value`}
        value={state.text}
        disabled={off}
        placeholder="JSON"
        onChange={(e) => set(e.target.value)}
        className="min-h-16 w-56 rounded-lg border border-input bg-background px-2 py-1 font-mono text-xs"
      />
    );
  } else {
    input = (
      <input
        aria-label={`${name} value`}
        value={state.text}
        disabled={off}
        inputMode={kind === "int" || kind === "float" ? "decimal" : undefined}
        placeholder={kind === "text" && /^(date|time|timestamp)/.test(col.type) ? "ISO text, e.g. 2026-10-02" : undefined}
        onChange={(e) => set(e.target.value)}
        className={`${cls} w-48`}
      />
    );
  }
  return (
    <span className="inline-flex flex-wrap items-center gap-2">
      {input}
      {col.nullable && (
        <label className="flex items-center gap-1 text-xs">
          <input
            type="checkbox"
            aria-label={`${name} NULL`}
            checked={state.isNull}
            disabled={state.useDefault}
            onChange={(e) => onChange({ ...state, isNull: e.target.checked })}
          />
          NULL
        </label>
      )}
      {allowDefault && col.has_default && (
        <label className="flex items-center gap-1 text-xs">
          <input
            type="checkbox"
            aria-label={`${name} use default`}
            checked={state.useDefault}
            onChange={(e) => onChange({ ...state, useDefault: e.target.checked })}
          />
          default
        </label>
      )}
    </span>
  );
}

export interface InsertFormProps {
  columns: Record<string, ColumnDesc>;
  enums: Record<string, string[]>;
  /** Returns an error text, or null when the insert went through. */
  onInsert: (values: Record<string, unknown>) => Promise<string | null>;
  onCancel: () => void;
}

export function InsertForm({ columns, enums, onInsert, onCancel }: InsertFormProps) {
  const names = Object.keys(columns).filter((n) => !columns[n].generated);
  const [fields, setFields] = useState<Record<string, FieldState>>(() =>
    Object.fromEntries(names.map((n) => [n, emptyField(columns[n])])),
  );
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const allDefault = names.every((n) => fields[n].useDefault);

  async function submit(event: FormEvent) {
    event.preventDefault();
    const values: Record<string, unknown> = {};
    for (const n of names) {
      const v = fieldValue(columns[n], fields[n]);
      if (!v.ok) return setError(`${n}: ${v.error}.`);
      if (v.value !== undefined) values[n] = v.value;
    }
    setBusy(true);
    setError(null);
    const failure = await onInsert(values);
    setBusy(false);
    if (failure) setError(failure);
  }

  return (
    <form onSubmit={(e) => void submit(e)} className="space-y-2 rounded-lg border p-3 text-sm">
      <h3 className="font-medium">Insert row</h3>
      {names.map((n) => (
        <div key={n} className="flex flex-wrap items-center gap-2">
          <span className="w-32 font-mono text-xs">
            {n} <span className="text-muted-foreground">{columns[n].type}</span>
          </span>
          <FieldInput
            name={n}
            col={columns[n]}
            enumValues={columns[n].enum ? (enums[columns[n].enum!] ?? enums[columns[n].enum!.split(".").pop()!]) : undefined}
            state={fields[n]}
            allowDefault
            onChange={(next) => setFields({ ...fields, [n]: next })}
          />
        </div>
      ))}
      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
      {allDefault && (
        <p id="insert-needs-value" className="text-muted-foreground">
          Give at least one column a value; a row can&apos;t be inserted with every column left to its default.
        </p>
      )}
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={busy || allDefault} aria-describedby={allDefault ? "insert-needs-value" : undefined}>
          Insert
        </Button>
        <Button type="button" size="sm" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}
