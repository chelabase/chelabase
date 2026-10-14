import { useContext, useEffect, useId, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { TableEditorStoreContext } from "@/lib/table-editor/store";
import { inputClass } from "./panel-bits";

/** A problem with one field of a dialog, shown under it once Add to changes was tried. */
export interface DialogError {
  field: string;
  message: string;
}

export const utf8Length = (s: string) => new TextEncoder().encode(s).length;

/** A name Postgres accepts as an identifier: not empty, at most 63 bytes. */
export const isIdentifier = (s: string) => s.trim() !== "" && utf8Length(s.trim()) <= 63;

/** The message for a Postgres identifier that is empty, too long or taken; null when fine. */
export function nameProblem(name: string, taken: (name: string) => boolean, what: string): string | null {
  if (name.trim() === "") return `Name the ${what}`;
  if (utf8Length(name) > 63) return "Postgres names are at most 63 bytes";
  if (taken(name)) return `${name} is already in use`;
  return null;
}

/**
 * A small form dialog whose button stages the edit: "Add to changes". The body is mounted only while
 * the dialog is open, so each opening starts from its initial state.
 */
export function EditDialog({
  title,
  description,
  submitLabel = "Add to changes",
  destructive = false,
  size,
  errors,
  onSubmit,
  onClose,
  children,
}: {
  title: string;
  description?: ReactNode;
  submitLabel?: string;
  destructive?: boolean;
  size?: "default" | "wide";
  /** The form's current problems; the dialog stays open while there are any. */
  errors: DialogError[];
  /** Called when there are no errors. */
  onSubmit: () => void;
  onClose: () => void;
  children: (show: (field: string) => string | undefined) => ReactNode;
}) {
  const [tried, setTried] = useState(false);
  // The pending changes' dry run waits while this dialog is open: it is about to add to them.
  const store = useContext(TableEditorStoreContext);
  useEffect(() => {
    store?.getState().setDialogOpen(true);
    return () => store?.getState().setDialogOpen(false);
  }, [store]);
  const show = (field: string) => (tried ? errors.find((e) => e.field === field)?.message : undefined);
  return (
    <Modal open onOpenChange={(open) => !open && onClose()} title={title} description={description} size={size}>
      <form
        className="space-y-3"
        noValidate
        onSubmit={(e) => {
          e.preventDefault();
          setTried(true);
          if (errors.length === 0) onSubmit();
        }}
      >
        {children(show)}
        <div className="flex justify-end gap-2 pt-1">
          <Button type="button" variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant={destructive ? "destructive" : "default"}>
            {submitLabel}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** A labelled text input with its error line. */
export function TextField({
  label,
  value,
  onChange,
  error,
  mono = true,
  multiline = false,
  placeholder,
  autoFocus,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  error?: string;
  mono?: boolean;
  multiline?: boolean;
  placeholder?: string;
  autoFocus?: boolean;
}) {
  const id = useId();
  const common = {
    id,
    value,
    autoFocus,
    placeholder,
    autoComplete: "off",
    spellCheck: false,
    "aria-invalid": error ? true : undefined,
    "aria-describedby": error ? `${id}-err` : undefined,
  } as const;
  const cls = mono ? `${inputClass} font-mono text-[13px]` : inputClass;
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-xs text-chalk-dim">
        {label}
      </label>
      {multiline ? (
        <textarea {...common} rows={4} onChange={(e) => onChange(e.target.value)} className={`${cls} h-auto py-1.5`} />
      ) : (
        <input {...common} onChange={(e) => onChange(e.target.value)} className={cls} />
      )}
      {error && (
        <p id={`${id}-err`} className="text-xs text-coral">
          {error}
        </p>
      )}
    </div>
  );
}

/** A labelled control (a Picker names itself through its own aria-label) with an error line. */
export function Field({ label, error, children }: { label: string; error?: string; children: ReactNode }) {
  return (
    <div className="space-y-1">
      <span aria-hidden="true" className="block text-xs text-chalk-dim">
        {label}
      </span>
      {children}
      {error && <p className="text-xs text-coral">{error}</p>}
    </div>
  );
}
