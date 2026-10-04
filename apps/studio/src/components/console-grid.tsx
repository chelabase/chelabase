import { useState } from "react";
import type { ColumnDesc } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import type { Sort } from "@/lib/console";
import { emptyField, fieldValue, FieldInput, toText, type FieldState } from "./row-editor";

export type Row = Record<string, unknown>;

export interface ConsoleGridProps {
  columns: Record<string, ColumnDesc>;
  enums: Record<string, string[]>;
  rows: Row[];
  pk: string[];
  readOnly: boolean;
  sort: Sort | null;
  onSort: (col: string) => void;
  selected: Set<string>;
  onToggle: (key: string) => void;
  /** Returns an error text, or null when the update went through. */
  onSave: (row: Row, col: string, value: unknown) => Promise<string | null>;
}

export const rowKey = (pk: string[], row: Row) => JSON.stringify(pk.map((c) => row[c]));
const rowLabel = (pk: string[], row: Row) => pk.map((c) => `${c} ${toText(row[c])}`).join(", ");

function Cell({ value }: { value: unknown }) {
  if (value === null || value === undefined) return <span className="text-muted-foreground italic">null</span>;
  if (typeof value === "object") return <code className="text-xs">{JSON.stringify(value)}</code>;
  return <span>{String(value)}</span>;
}

export function ConsoleGrid({ columns, enums, rows, pk, readOnly, sort, onSort, selected, onToggle, onSave }: ConsoleGridProps) {
  const names = Object.keys(columns);
  const [editing, setEditing] = useState<{ key: string; col: string } | null>(null);
  const [field, setField] = useState<FieldState>({ text: "", isNull: false, useDefault: false });
  const [error, setError] = useState<string | null>(null);

  function startEdit(row: Row, col: string) {
    const v = row[col];
    setEditing({ key: rowKey(pk, row), col });
    setField({ ...emptyField(columns[col]), useDefault: false, isNull: v === null, text: toText(v, columns[col]) });
    setError(null);
  }
  async function save(row: Row, col: string) {
    const v = fieldValue(columns[col], field);
    if (!v.ok) return setError(`${col}: ${v.error}.`);
    let failure: string | null;
    try {
      failure = await onSave(row, col, v.value);
    } catch (err) {
      failure = err instanceof Error ? err.message : "Something went wrong";
    }
    if (failure) setError(failure);
    else setEditing(null);
  }

  return (
    <table className="w-full text-left text-sm">
      <thead className="border-b text-muted-foreground">
        <tr>
          {!readOnly && <th className="py-1" />}
          {names.map((n) => (
            <th key={n} className="py-1 font-medium">
              <button type="button" aria-label={`Sort by ${n}`} onClick={() => onSort(n)} className="font-medium">
                {n}
                {sort?.col === n ? (sort.dir === "asc" ? " ▲" : " ▼") : ""}
              </button>
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.length === 0 && (
          <tr>
            <td colSpan={names.length + 1} className="py-2 text-muted-foreground">
              No rows.
            </td>
          </tr>
        )}
        {rows.map((row, i) => {
          const key = rowKey(pk, row);
          return (
            <tr key={readOnly ? i : key} className="border-b last:border-0">
              {!readOnly && (
                <td className="py-1 pr-2">
                  <input type="checkbox" aria-label={`Select row ${rowLabel(pk, row)}`} checked={selected.has(key)} onChange={() => onToggle(key)} />
                </td>
              )}
              {names.map((n) => (
                <td key={n} className="py-1 pr-3 align-top">
                  {editing?.key === key && editing.col === n ? (
                    <div className="space-y-1">
                      <FieldInput
                        name={n}
                        col={columns[n]}
                        enumValues={columns[n].enum ? (enums[columns[n].enum!] ?? enums[columns[n].enum!.split(".").pop()!]) : undefined}
                        state={field}
                        onChange={setField}
                      />
                      <div className="flex gap-1">
                        <Button size="xs" onClick={() => void save(row, n)}>
                          Save
                        </Button>
                        <Button size="xs" variant="outline" onClick={() => {
                            setEditing(null);
                            setError(null);
                          }}>
                          Cancel
                        </Button>
                      </div>
                      {error && (
                        <p role="alert" className="text-xs text-destructive">
                          {error}
                        </p>
                      )}
                    </div>
                  ) : (
                    <span className="inline-flex items-center gap-1">
                      <Cell value={row[n]} />
                      {!readOnly && !pk.includes(n) && !columns[n].generated && (
                        <button type="button" aria-label={`Edit ${n}, ${rowLabel(pk, row)}`} onClick={() => startEdit(row, n)} className="text-xs text-muted-foreground hover:underline">
                          edit
                        </button>
                      )}
                    </span>
                  )}
                </td>
              ))}
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
