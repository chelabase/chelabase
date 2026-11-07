import { useEffect, useId, useState, type FormEvent } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { storageCopyObject, storageDeleteObject, storageListObjects, storageMoveObject } from "@chelabase/management-client";
import { CopyIconButton } from "@/components/copy-button";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Modal } from "@/components/ui/dialog";
import { Select } from "@/components/ui/select";
import { MAX_SIGNED_TTL, objectActionError, parseObjectPage, storageParams } from "@/lib/storage-api";
import { baseName, folderPathOf, folderProblem, joinKey, nameProblem, parentPath, prefixOf } from "./paths";
import { deleteReport, type DeleteReport, type DeleteResult } from "./selection";

/** Where an object action runs: the branch, the Act as role and the bucket. */
export interface ObjectPlace {
  databaseId: string;
  branchId: string;
  role: string | null;
  bucket: string;
}

const input = "h-8 rounded-lg border border-input bg-background px-2";

/** Deletes run this many at a time. */
const DELETE_CONCURRENCY = 6;

/** Deletes each object on its own (so each failure has its own reason), a few at a time. */
export async function deleteEach(place: ObjectPlace, names: readonly string[]): Promise<DeleteResult[]> {
  const results: DeleteResult[] = new Array(names.length);
  let next = 0;
  const worker = async () => {
    while (next < names.length) {
      const i = next++;
      const name = names[i];
      try {
        await storageDeleteObject(place.databaseId, place.branchId, place.bucket, name, storageParams(place.role));
        results[i] = { name, ok: true };
      } catch (err) {
        results[i] = { name, ok: false, message: objectActionError(err) };
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(DELETE_CONCURRENCY, names.length) }, worker));
  return results;
}

const count = (n: number) => `${n} ${n === 1 ? "object" : "objects"}`;

export function DeleteObjectsDialog({
  open,
  onOpenChange,
  place,
  names,
  onDone,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  place: ObjectPlace;
  names: readonly string[];
  onDone: (report: DeleteReport) => void;
}) {
  const remove = useMutation({ mutationFn: () => deleteEach(place, names) });
  useEffect(() => {
    if (open) remove.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);
  const label = `Delete ${count(names.length)}`;
  return (
    <ConfirmDialog
      open={open}
      onOpenChange={onOpenChange}
      title={`${label}?`}
      body={
        <p>
          {names.length === 1 ? (
            <>
              <span className="font-mono">{baseName(names[0])}</span> will be deleted.
            </>
          ) : (
            `${count(names.length)} will be deleted.`
          )}{" "}
          Signed URLs to {names.length === 1 ? "it" : "them"} stop working. This can&apos;t be undone.
        </p>
      }
      confirm={label}
      tone="destructive"
      busy={remove.isPending}
      onConfirm={() =>
        remove.mutate(undefined, {
          onSuccess: (results) => {
            onOpenChange(false);
            onDone(deleteReport(results));
          },
        })
      }
    />
  );
}

export function RenameDialog({
  open,
  onOpenChange,
  place,
  name,
  onDone,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  place: ObjectPlace;
  name: string;
  onDone: (to: string) => void;
}) {
  const [value, setValue] = useState(baseName(name));
  const move = useMutation({
    mutationFn: (to: string) => storageMoveObject(place.databaseId, place.branchId, { bucket: place.bucket, from: name, to }, storageParams(place.role)),
  });
  useEffect(() => {
    if (open) {
      setValue(baseName(name));
      move.reset();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, name]);
  const problem = nameProblem(value);
  const to = joinKey(parentPath(name), value);
  const unchanged = to === name;
  function submit(event: FormEvent) {
    event.preventDefault();
    if (problem || unchanged || move.isPending) return;
    move.mutate(to, {
      onSuccess: () => {
        onOpenChange(false);
        onDone(to);
      },
    });
  }
  return (
    <Modal open={open} onOpenChange={(next) => !move.isPending && onOpenChange(next)} title={`Rename ${baseName(name)}`} description="The file stays in its folder. Signed URLs to the old name stop working.">
      <form onSubmit={submit} className="space-y-4">
        <label className="flex flex-col gap-1 text-sm">
          <span>Name</span>
          <input value={value} onChange={(e) => setValue(e.target.value)} autoComplete="off" spellCheck={false} className={`${input} font-mono`} />
        </label>
        {problem && value !== "" && <p className="text-xs text-coral">{problem}</p>}
        {move.error && (
          <p role="alert" className="text-sm text-coral">
            {objectActionError(move.error)}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" disabled={move.isPending} onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" disabled={!!problem || unchanged || move.isPending}>
            {move.isPending ? "Renaming…" : "Rename"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** The folders directly under a folder path (the first page), for the folder picker. */
function useSubfolders(place: ObjectPlace, path: string, enabled: boolean) {
  return useQuery({
    queryKey: ["storage-folder-picker", place.databaseId, place.branchId, place.bucket, path, storageParams(place.role)],
    queryFn: async ({ signal }) =>
      parseObjectPage(
        await storageListObjects(place.databaseId, place.branchId, place.bucket, { prefix: prefixOf(path), delimiter: "/", limit: 1000 }, storageParams(place.role), { signal }),
      ).items.filter((i) => i.folder),
    enabled,
    retry: false,
    staleTime: 30_000,
  });
}

/** Move or copy one object: a folder (typed, or picked from the folders under it) and a name. */
export function RelocateDialog({
  open,
  onOpenChange,
  place,
  name,
  mode,
  onDone,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  place: ObjectPlace;
  name: string;
  mode: "move" | "copy";
  onDone: (to: string) => void;
}) {
  const ids = useId();
  const [folder, setFolder] = useState(parentPath(name));
  const [newName, setNewName] = useState(baseName(name));
  const relocate = useMutation({
    mutationFn: (to: string) => {
      const body = { bucket: place.bucket, from: name, to };
      return mode === "move"
        ? storageMoveObject(place.databaseId, place.branchId, body, storageParams(place.role))
        : storageCopyObject(place.databaseId, place.branchId, body, storageParams(place.role));
    },
  });
  useEffect(() => {
    if (open) {
      setFolder(parentPath(name));
      setNewName(baseName(name));
      relocate.reset();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, name]);
  const cleanFolder = folder.split("/").filter(Boolean).join("/");
  const folderError = folderProblem(folder);
  const subfolders = useSubfolders(place, cleanFolder, open && !folderError);
  const problem = folderError ?? nameProblem(newName);
  const to = joinKey(cleanFolder, newName);
  const same = to === name;
  const verb = mode === "move" ? "Move" : "Copy";
  function submit(event: FormEvent) {
    event.preventDefault();
    if (problem || same || relocate.isPending) return;
    relocate.mutate(to, {
      onSuccess: () => {
        onOpenChange(false);
        onDone(to);
      },
    });
  }
  return (
    <Modal
      open={open}
      onOpenChange={(next) => !relocate.isPending && onOpenChange(next)}
      title={`${verb} ${baseName(name)}`}
      description={mode === "move" ? "Signed URLs to the old path stop working." : "The copy is a new file; the original stays."}
      size="medium"
    >
      <form onSubmit={submit} className="space-y-4">
        <label className="flex flex-col gap-1 text-sm">
          <span>Folder</span>
          <input
            value={folder}
            onChange={(e) => setFolder(e.target.value)}
            placeholder="The bucket's root"
            autoComplete="off"
            spellCheck={false}
            aria-describedby={`${ids}-folders`}
            className={`${input} font-mono`}
          />
        </label>
        <div id={`${ids}-folders`} className="space-y-1 text-sm">
          <div className="flex items-center gap-2 text-xs text-chalk-dim">
            <span>In {cleanFolder === "" ? "the bucket's root" : <span className="font-mono">{cleanFolder}</span>}:</span>
            {cleanFolder !== "" && (
              <button type="button" className="text-sand underline-offset-4 hover:underline" onClick={() => setFolder(parentPath(cleanFolder))}>
                Up a folder
              </button>
            )}
          </div>
          <ul aria-label="Folders" className="max-h-40 overflow-y-auto border border-line">
            {subfolders.data?.length === 0 && <li className="px-3 py-1.5 text-xs text-chalk-dim">No folders here. Type a new one to create it.</li>}
            {subfolders.data?.map((f) => (
              <li key={f.name}>
                <button type="button" className="w-full px-3 py-1.5 text-left font-mono hover:bg-chalk/[0.06]" onClick={() => setFolder(folderPathOf(f.name))}>
                  {baseName(f.name)}/
                </button>
              </li>
            ))}
          </ul>
        </div>
        <label className="flex flex-col gap-1 text-sm">
          <span>Name</span>
          <input value={newName} onChange={(e) => setNewName(e.target.value)} autoComplete="off" spellCheck={false} className={`${input} font-mono`} />
        </label>
        {problem && newName !== "" && <p className="text-xs text-coral">{problem}</p>}
        {same && <p className="text-xs text-chalk-dim">Pick another folder or name.</p>}
        {relocate.error && (
          <p role="alert" className="text-sm text-coral">
            {objectActionError(relocate.error)}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" disabled={relocate.isPending} onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" disabled={!!problem || same || relocate.isPending}>
            {relocate.isPending ? "Working…" : verb}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** The lifetimes a console signed URL may have (at most an hour). */
export const TTL_CHOICES: { seconds: number; label: string }[] = [
  { seconds: 60, label: "1 minute" },
  { seconds: 300, label: "5 minutes" },
  { seconds: 900, label: "15 minutes" },
  { seconds: 3600, label: "1 hour" },
].filter((c) => c.seconds <= MAX_SIGNED_TTL);

/** Mints a signed URL with a picked lifetime and shows it to copy. */
export function SignDialog({
  open,
  onOpenChange,
  name,
  sign,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  name: string;
  sign: (expiresIn: number) => Promise<{ url: string; expiresAt: number | null }>;
}) {
  const [ttl, setTtl] = useState(3600);
  const mint = useMutation({ mutationFn: (seconds: number) => sign(seconds) });
  useEffect(() => {
    if (open) mint.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);
  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      title={`Signed URL for ${baseName(name)}`}
      description="Anyone with the URL can download this version of the file until it expires. An overwrite or a delete ends it."
      size="medium"
    >
      <div className="space-y-4 text-sm">
        <div className="flex items-end gap-2">
          <label className="flex flex-col gap-1">
            <span>Expires after</span>
            <Select value={String(ttl)} onChange={(e) => setTtl(Number(e.target.value))} wrapperClassName="w-40">
              {TTL_CHOICES.map((c) => (
                <option key={c.seconds} value={c.seconds}>
                  {c.label}
                </option>
              ))}
            </Select>
          </label>
          <Button type="button" disabled={mint.isPending} onClick={() => mint.mutate(ttl)}>
            {mint.isPending ? "Creating…" : "Create URL"}
          </Button>
        </div>
        {mint.data && (
          <div className="space-y-1">
            <div className="flex">
              <input readOnly value={mint.data.url} aria-label="Signed URL" onFocus={(e) => e.currentTarget.select()} className={`${input} min-w-0 flex-1 rounded-r-none font-mono text-xs`} />
              <CopyIconButton value={mint.data.url} label="Copy signed URL to the clipboard" />
            </div>
            {mint.data.expiresAt !== null && (
              <p className="text-xs text-chalk-dim">Expires {new Date(mint.data.expiresAt * 1000).toLocaleString("en-GB", { dateStyle: "medium", timeStyle: "short" })}.</p>
            )}
          </div>
        )}
        {mint.error && (
          <p role="alert" className="text-coral">
            {objectActionError(mint.error)}
          </p>
        )}
        <div className="flex justify-end">
          <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
            Done
          </Button>
        </div>
      </div>
    </Modal>
  );
}
