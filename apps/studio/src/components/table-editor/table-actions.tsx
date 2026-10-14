import { useState } from "react";
import { useListObjects } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { schemasOf } from "@/lib/table-editor/context";
import { useTableEditor } from "@/lib/table-editor/store";
import { RenameDialog } from "./constraints-section";
import { EditDialog, Field, isIdentifier, TextField } from "./edit-dialog";
import { Picker } from "./picker";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";

type Action = "rename" | "move" | "comment" | "drop";

/** The Table section's actions: Rename table, Move to schema, Comment, Drop table. Each opens a small dialog. */
export function TableActions({ target, reason }: { target: TableTarget; reason: string | null }) {
  const { projected, stage, applying, reason: dropped } = useTableStructure(target);
  const openPanel = useTableEditor((s) => s.openPanel);
  const [open, setOpen] = useState<Action | null>(null);
  const params = target.database ? { database: target.database } : undefined;
  const objects = useListObjects(target.databaseId, target.branchId, params);
  if (!projected) return null;
  // Operations after a rename or a move name the table as it is by then.
  const ref = { schema: projected.schema, name: projected.name };
  const disabled = reason !== null || applying || dropped !== null;
  const schemas = schemasOf(objects.data?.objects ?? []).filter((s) => s !== projected.schema);
  const taken = (schema: string, name: string) => (objects.data?.objects ?? []).some((o) => o.schema === schema && o.name === name);
  const done = (ops: Parameters<typeof stage>[0]) => {
    stage(ops);
    openPanel("changes");
  };
  const close = () => setOpen(null);
  const button = (id: Action, label: string, danger = false) => (
    <Button key={id} size="sm" variant={danger ? "destructive" : "outline"} disabled={disabled} title={reason ?? dropped ?? undefined} onClick={() => setOpen(id)}>
      {label}
    </Button>
  );
  return (
    <div className="flex flex-wrap gap-2">
      {button("rename", "Rename table")}
      {button("move", "Move to schema")}
      {button("comment", "Comment")}
      {button("drop", "Drop table", true)}

      {open === "rename" && (
        <RenameDialog
          title="Rename table"
          description={`${projected.schema}.${projected.name}`}
          what="table"
          current={projected.name}
          taken={(to) => taken(projected.schema, to)}
          onClose={close}
          onSubmit={(to) => done([{ rename_table: { table: ref, to } }])}
        />
      )}
      {open === "move" && <MoveDialog schemas={schemas} current={projected.schema} onClose={close} onSubmit={(schema) => done([{ set_table_schema: { table: ref, schema } }])} taken={(schema) => taken(schema, projected.name)} name={projected.name} />}
      {open === "comment" && <CommentDialog initial={projected.comment ?? ""} onClose={close} onSubmit={(comment) => done([{ set_table_comment: { table: ref, comment: comment === "" ? null : comment } }])} />}
      {open === "drop" && (
        <EditDialog
          title="Drop table"
          description={`${projected.schema}.${projected.name} and all its rows are deleted when you apply. Objects that depend on it are listed in the changes before then.`}
          destructive
          errors={[]}
          onClose={close}
          onSubmit={() => {
            done([{ drop_table: { table: ref } }]);
            close();
          }}
        >
          {() => null}
        </EditDialog>
      )}
    </div>
  );
}

/** Move to schema: a listed schema, or a typed name (a schema that holds no object yet isn't listed). */
function MoveDialog({
  schemas,
  current,
  name,
  taken,
  onSubmit,
  onClose,
}: {
  schemas: string[];
  current: string;
  name: string;
  taken: (schema: string) => boolean;
  onSubmit: (schema: string) => void;
  onClose: () => void;
}) {
  const [schema, setSchema] = useState<string | null>(null);
  const errors =
    schema === null
      ? [{ field: "schema", message: "Pick a schema" }]
      : !isIdentifier(schema)
        ? [{ field: "schema", message: "Postgres names are at most 63 bytes" }]
        : schema === current
          ? [{ field: "schema", message: `${name} is already in ${current}` }]
          : taken(schema)
            ? [{ field: "schema", message: `${schema} already has a ${name}` }]
            : [];
  return (
    <EditDialog
      title="Move to schema"
      errors={errors}
      onClose={onClose}
      onSubmit={() => {
        onSubmit(schema!);
        onClose();
      }}
    >
      {(show) => (
        <Field label="Schema" error={show("schema")}>
          <Picker
            label="Schema"
            look="field"
            mono
            options={schemas.map((s) => ({ value: s, label: s }))}
            value={schema}
            onChange={setSchema}
            acceptTyped={(text) => isIdentifier(text) && text !== current}
            placeholder="Pick or type a schema"
          />
        </Field>
      )}
    </EditDialog>
  );
}

function CommentDialog({ initial, onSubmit, onClose }: { initial: string; onSubmit: (comment: string) => void; onClose: () => void }) {
  const [comment, setComment] = useState(initial);
  return (
    <EditDialog
      title="Comment"
      errors={[]}
      onClose={onClose}
      onSubmit={() => {
        onSubmit(comment.trim());
        onClose();
      }}
    >
      {() => <TextField label="Comment" autoFocus mono={false} multiline value={comment} onChange={setComment} />}
    </EditDialog>
  );
}
