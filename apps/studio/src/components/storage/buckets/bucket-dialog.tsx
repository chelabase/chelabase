import { useEffect, useId, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useStorageCreateBucket, useStorageUpdateBucket } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { errorText } from "@/lib/errors";
import { invalidateStorage, storageParams, type Bucket } from "@/lib/storage-api";
import { BLANK_BUCKET_FORM, SIZE_UNITS, bucketFormFrom, parseBucketForm, type BucketFormInput, type SizeUnit } from "./bucket-form";

export interface BucketDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  databaseId: string;
  branchId: string;
  /** The Act as role; null is owner mode. */
  role: string | null;
  /** The bucket being edited; absent for a new one. */
  bucket?: Bucket;
  /** After a create: the new bucket's name. */
  onCreated?: (name: string) => void;
}

const input = "h-8 rounded-lg border border-input bg-background px-2";

/**
 * Create a bucket (name, public, size limit, allowed types) or edit one's settings. A refusal
 * (a taken name, a policy) shows the server's message and the dialog stays open.
 */
export function BucketDialog({ open, onOpenChange, databaseId, branchId, role, bucket, onCreated }: BucketDialogProps) {
  const queryClient = useQueryClient();
  const [form, setForm] = useState<BucketFormInput>(BLANK_BUCKET_FORM);
  const ids = { error: useId(), types: useId() };
  const done = async () => {
    await invalidateStorage(queryClient, databaseId, branchId);
    onOpenChange(false);
  };
  const create = useStorageCreateBucket({
    mutation: {
      onSuccess: async (_, { data }) => {
        await done();
        onCreated?.((data as { name: string }).name);
      },
    },
  });
  const update = useStorageUpdateBucket({ mutation: { onSuccess: done } });
  const mutation = bucket ? update : create;

  useEffect(() => {
    if (open) {
      setForm(bucket ? bucketFormFrom(bucket) : BLANK_BUCKET_FORM);
      create.reset();
      update.reset();
    }
    // Reset on each opening only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const parsed = parseBucketForm(form, bucket ?? null);
  const set = <K extends keyof BucketFormInput>(key: K, value: BucketFormInput[K]) => setForm((f) => ({ ...f, [key]: value }));
  const busy = mutation.isPending;
  // A field's problem shows once it has something in it (or, for the size limit, when it was emptied).
  const problems = parsed.ok ? [] : [form.name.trim() !== "" ? parsed.name : undefined, parsed.sizeLimit, parsed.allowedTypes].filter((m): m is string => !!m);

  function submit(event: FormEvent) {
    event.preventDefault();
    if (!parsed.ok || busy) return;
    const params = storageParams(role);
    if (!bucket) {
      create.mutate({ id: databaseId, branchId, data: parsed.body, params });
    } else if (Object.keys(parsed.body).length === 0) {
      onOpenChange(false);
    } else {
      update.mutate({ id: databaseId, branchId, name: bucket.name, data: parsed.body, params });
    }
  }

  return (
    <Modal
      open={open}
      onOpenChange={(next) => !busy && onOpenChange(next)}
      title={bucket ? `Edit ${bucket.name}` : "New bucket"}
      description={bucket ? "A size limit can be raised or lowered, not removed." : "A bucket holds files under one set of access rules."}
    >
      <form onSubmit={submit} className="space-y-4" aria-describedby={problems.length > 0 ? ids.error : undefined}>
        {!bucket && (
          <label className="flex flex-col gap-1 text-sm">
            <span>Name</span>
            <input value={form.name} onChange={(e) => set("name", e.target.value)} autoComplete="off" spellCheck={false} className={`${input} font-mono`} />
          </label>
        )}
        <div className="space-y-1 text-sm">
          <label className="flex items-center gap-2">
            <input type="checkbox" checked={form.public} onChange={(e) => set("public", e.target.checked)} />
            <span>Public</span>
          </label>
          <p className="text-xs text-chalk-dim">Anyone with a file&apos;s URL can download it. Private files need a signed URL or a key.</p>
        </div>
        <div className="flex items-end gap-2">
          <label className="flex flex-col gap-1 text-sm">
            <span>Size limit</span>
            <input type="number" min={1} value={form.sizeLimit} onChange={(e) => set("sizeLimit", e.target.value)} placeholder="None" className={`${input} w-32`} />
          </label>
          <label className="flex flex-col gap-1 text-sm">
            <span className="sr-only">Size unit</span>
            <select value={form.sizeUnit} onChange={(e) => set("sizeUnit", e.target.value as SizeUnit)} className={input}>
              {SIZE_UNITS.map((u) => (
                <option key={u} value={u}>
                  {u === "B" ? "bytes" : u}
                </option>
              ))}
            </select>
          </label>
        </div>
        <div className="flex flex-col gap-1 text-sm">
          <label className="flex flex-col gap-1">
          <span>Allowed types</span>
          <textarea
            value={form.allowedTypes}
            onChange={(e) => set("allowedTypes", e.target.value)}
            rows={2}
            spellCheck={false}
            placeholder="Any type"
            aria-describedby={ids.types}
            className="rounded-lg border border-input bg-background px-2 py-1 font-mono"
          />
          </label>
          <span id={ids.types} className="text-xs text-chalk-dim">
            Comma-separated, such as image/*, application/pdf. Blank allows any type.
          </span>
        </div>
        {problems.length > 0 && (
          <div id={ids.error} className="space-y-1">
            {problems.map((m) => (
              <p key={m} className="text-xs text-coral">
                {m}
              </p>
            ))}
          </div>
        )}
        {mutation.error && (
          <p role="alert" className="text-sm text-coral">
            {errorText(mutation.error)}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" disabled={busy} onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" disabled={!parsed.ok || busy}>
            {busy ? "Saving…" : bucket ? "Save" : "Create bucket"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}
