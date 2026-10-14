import { useEffect, useRef, useState } from "react";
import { Plus } from "lucide-react";
import type { Operation } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { opsOf, type Mark } from "@/lib/table-editor/changeset";
import { projectEnums, type ProjectedEnum } from "@/lib/table-editor/enums";
import { useTableEditor } from "@/lib/table-editor/store";
import { cn } from "@/lib/utils";
import { RenameDialog } from "./constraints-section";
import { EditDialog, Field, nameProblem, TextField, utf8Length } from "./edit-dialog";
import { Picker } from "./picker";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";

type Dialog = { kind: "new" } | { kind: "add_value" | "rename_value" | "rename"; enum: string };

const actionClass =
  "px-1 text-xs text-chalk-dim underline decoration-line-strong underline-offset-[3px] outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand disabled:no-underline disabled:opacity-50";

function MarkTag({ mark }: { mark: Mark | null }) {
  if (!mark || mark === "changed") return null;
  return <span className={cn("ml-2 font-sans text-[11px]", mark === "dropped" ? "text-coral" : "text-sand")}>{mark === "added" ? "new" : mark}</span>;
}

/**
 * The schema's enums with their values, and the actions on them: New enum, Add value, Rename value,
 * Rename, Drop. Every action is a pending change in the schema's `#types` changeset.
 */
export function TypesView({ target, reason }: { target: TableTarget; reason: string | null }) {
  const { catalog, edits, stage, applying } = useTableStructure(target);
  const openPanel = useTableEditor((s) => s.openPanel);
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const { schema } = target;
  const enums = projectEnums(catalog, opsOf(edits), schema);
  const disabled = reason !== null || applying;
  const close = () => setDialog(null);
  const done = (ops: Operation[]) => {
    stage(ops);
    openPanel("changes");
  };
  // The Apply outcome, as the table view shows it (M5): a live region from the start that takes the focus Apply leaves.
  const applied = useTableEditor((s) => (s.applied?.key === target.key ? s.applied.text : null));
  const statusRef = useRef<HTMLParagraphElement>(null);
  useEffect(() => {
    const active = document.activeElement;
    if (applied && (!active || active === document.body || !active.isConnected)) statusRef.current?.focus();
  }, [applied]);
  const byName = (name: string) => enums.find((e) => e.name === name && e.mark !== "dropped");
  // A type name is taken by any type of the schema, an enum or not.
  const typeTaken = (name: string) => enums.some((e) => e.name === name && e.mark !== "dropped") || catalog.some((t) => t.schema === schema && t.name === name);

  return (
    <section aria-label={`Types in ${schema}`} className="min-h-0 flex-1 space-y-6 overflow-auto p-4">
      <div className="flex items-center gap-3">
        <div className="min-w-0">
          <h2 className="text-lg leading-6 font-medium">Types in {schema}</h2>
          {/* Its own line under the heading (empty, it takes no room). */}
          <p ref={statusRef} role="status" tabIndex={-1} className={cn("flex items-center gap-2 text-[13px] text-chalk outline-none", applied && "mt-1")}>
            {applied && (
              <>
                <span aria-hidden="true" className="size-2 shrink-0 bg-seafoam" />
                <span className="truncate">{applied}</span>
              </>
            )}
          </p>
        </div>
        <Button size="sm" variant="outline" className="ml-auto" disabled={disabled} title={reason ?? undefined} onClick={() => setDialog({ kind: "new" })}>
          <Plus aria-hidden="true" strokeWidth={1.5} />
          New enum
        </Button>
      </div>
      {reason && <p className="text-[13px] text-chalk-dim">{reason}</p>}
      {enums.length === 0 && <p className="text-[13px] text-chalk-dim">No enums in {schema}</p>}
      {enums.map((e) => (
        <section key={e.original ?? `+${e.name}`} aria-label={e.name} className={cn("space-y-2 border-b border-line pb-4", e.mark === "dropped" && "text-chalk-dim")}>
          <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
            <h3 className={cn("font-mono text-sm font-semibold text-chalk", e.mark === "dropped" && "line-through decoration-coral")}>{e.name}</h3>
            <MarkTag mark={e.mark} />
            {e.mark !== "dropped" && (
              <span className="ml-auto flex items-baseline gap-2">
                <button type="button" aria-label={`Add value to ${e.name}`} disabled={disabled} onClick={() => setDialog({ kind: "add_value", enum: e.name })} className={actionClass}>
                  Add value
                </button>
                <button type="button" aria-label={`Rename value in ${e.name}`} disabled={disabled || e.values.length === 0} onClick={() => setDialog({ kind: "rename_value", enum: e.name })} className={actionClass}>
                  Rename value
                </button>
                <button type="button" aria-label={`Rename ${e.name}`} disabled={disabled} onClick={() => setDialog({ kind: "rename", enum: e.name })} className={actionClass}>
                  Rename
                </button>
                <button type="button" aria-label={`Drop ${e.name}`} disabled={disabled} onClick={() => done([{ drop_enum: { schema, name: e.name } }])} className={actionClass}>
                  Drop
                </button>
              </span>
            )}
          </div>
          <ul className="flex flex-wrap gap-x-4 gap-y-1 font-mono text-[13px]">
            {e.values.map((v) => (
              <li key={v.value} className={cn(e.mark === "dropped" ? "text-chalk-dim" : "text-chalk")}>
                <span>{v.value}</span>
                <MarkTag mark={v.mark} />
              </li>
            ))}
            {e.values.length === 0 && <li className="font-sans text-chalk-dim">No values yet</li>}
          </ul>
        </section>
      ))}

      {dialog?.kind === "new" && <NewEnumDialog taken={typeTaken} onClose={close} onSubmit={(name, values) => done([{ create_enum: { schema, name, values } }])} />}
      {dialog?.kind === "add_value" && byName(dialog.enum) && (
        <AddValueDialog en={byName(dialog.enum)!} onClose={close} onSubmit={(value, position) => done([{ add_enum_value: { schema, name: dialog.enum, value, ...(position ? { position } : {}) } }])} />
      )}
      {dialog?.kind === "rename_value" && byName(dialog.enum) && (
        <RenameValueDialog en={byName(dialog.enum)!} onClose={close} onSubmit={(from, to) => done([{ rename_enum_value: { schema, name: dialog.enum, from, to } }])} />
      )}
      {dialog?.kind === "rename" && (
        <RenameDialog
          title={`Rename ${dialog.enum}`}
          what="enum"
          current={dialog.enum}
          taken={typeTaken}
          onClose={close}
          onSubmit={(to) => done([{ rename_enum: { schema, name: dialog.enum, to } }])}
        />
      )}
    </section>
  );
}

function NewEnumDialog({ taken, onSubmit, onClose }: { taken: (name: string) => boolean; onSubmit: (name: string, values: string[]) => void; onClose: () => void }) {
  const [name, setName] = useState("");
  const [text, setText] = useState("");
  const values = text.split("\n").map((v) => v.trim()).filter((v) => v !== "");
  const dup = values.find((v, i) => values.indexOf(v) !== i);
  const errors = [
    ...(nameProblem(name.trim(), taken, "enum") ? [{ field: "name", message: nameProblem(name.trim(), taken, "enum")! }] : []),
    ...(dup !== undefined ? [{ field: "values", message: `${dup} is listed twice` }] : []),
    ...(values.some((v) => utf8Length(v) > 63) ? [{ field: "values", message: "Enum values are at most 63 bytes" }] : []),
  ];
  return (
    <EditDialog
      title="New enum"
      errors={errors}
      onClose={onClose}
      onSubmit={() => {
        onSubmit(name.trim(), values);
        onClose();
      }}
    >
      {(show) => (
        <>
          <TextField label="Enum name" autoFocus value={name} onChange={setName} error={show("name")} />
          <TextField label="Values" multiline value={text} onChange={setText} error={show("values")} placeholder="One value per line" />
        </>
      )}
    </EditDialog>
  );
}

type Where = "end" | "before" | "after";
const WHERE: { value: Where; label: string }[] = [
  { value: "end", label: "At the end" },
  { value: "before", label: "Before…" },
  { value: "after", label: "After…" },
];

function AddValueDialog({ en, onSubmit, onClose }: { en: ProjectedEnum; onSubmit: (value: string, position: { before: string } | { after: string } | null) => void; onClose: () => void }) {
  const [value, setValue] = useState("");
  const [where, setWhere] = useState<Where>("end");
  const [rel, setRel] = useState<string | null>(null);
  const v = value.trim();
  const errors = [
    ...(v === "" ? [{ field: "value", message: "Write the value" }] : []),
    ...(utf8Length(v) > 63 ? [{ field: "value", message: "Enum values are at most 63 bytes" }] : []),
    ...(en.values.some((x) => x.value === v) ? [{ field: "value", message: `${v} is already a value` }] : []),
    ...(where !== "end" && rel === null ? [{ field: "rel", message: "Pick the value to place it by" }] : []),
  ];
  return (
    <EditDialog
      title={`Add value to ${en.name}`}
      errors={errors}
      onClose={onClose}
      onSubmit={() => {
        onSubmit(v, where === "end" ? null : where === "before" ? { before: rel! } : { after: rel! });
        onClose();
      }}
    >
      {(show) => (
        <>
          <TextField label="Value" autoFocus value={value} onChange={setValue} error={show("value")} />
          <Field label="Position">
            <Picker label="Position" look="field" options={WHERE} value={where} onChange={(w) => setWhere(w as Where)} />
          </Field>
          {where !== "end" && (
            <Field label="Relative to" error={show("rel")}>
              <Picker label="Relative to" look="field" mono options={en.values.map((x) => ({ value: x.value, label: x.value }))} value={rel} onChange={setRel} placeholder="Pick a value" />
            </Field>
          )}
        </>
      )}
    </EditDialog>
  );
}

function RenameValueDialog({ en, onSubmit, onClose }: { en: ProjectedEnum; onSubmit: (from: string, to: string) => void; onClose: () => void }) {
  const [from, setFrom] = useState<string | null>(null);
  const [to, setTo] = useState("");
  const name = to.trim();
  const errors = [
    ...(from === null ? [{ field: "from", message: "Pick the value to rename" }] : []),
    ...(name === "" ? [{ field: "to", message: "Write the new value" }] : []),
    ...(utf8Length(name) > 63 ? [{ field: "to", message: "Enum values are at most 63 bytes" }] : []),
    ...(en.values.some((x) => x.value === name) ? [{ field: "to", message: `${name} is already a value` }] : []),
  ];
  return (
    <EditDialog
      title={`Rename value in ${en.name}`}
      errors={errors}
      onClose={onClose}
      onSubmit={() => {
        onSubmit(from!, name);
        onClose();
      }}
    >
      {(show) => (
        <>
          <Field label="From" error={show("from")}>
            <Picker label="From" look="field" mono options={en.values.map((x) => ({ value: x.value, label: x.value }))} value={from} onChange={setFrom} placeholder="Pick a value" />
          </Field>
          <TextField label="To" value={to} onChange={setTo} error={show("to")} />
        </>
      )}
    </EditDialog>
  );
}
