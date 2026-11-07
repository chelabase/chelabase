/**
 * What a drop, a paste or a file input holds, as files with the relative paths to upload them
 * under. A dropped folder is walked through `webkitGetAsEntry` (the File and Directory Entries
 * API) and its folders become key prefixes; empty folders add nothing, since folders in a bucket
 * are only prefixes.
 */

export interface DroppedFile {
  /** Relative to the drop: `a/b/c.txt` for `c.txt` in a dropped folder `a`. */
  path: string;
  file: File;
}

/** The parts of the entries API used here (browsers expose more). */
interface Entry {
  isFile: boolean;
  isDirectory: boolean;
  name: string;
}
interface FileEntry extends Entry {
  file(ok: (f: File) => void, fail?: (e: unknown) => void): void;
}
interface DirectoryEntry extends Entry {
  createReader(): { readEntries(ok: (entries: Entry[]) => void, fail?: (e: unknown) => void): void };
}

/** A `DataTransferItem` as far as this needs it. */
export interface DropItem {
  kind: string;
  getAsFile(): File | null;
  webkitGetAsEntry?: () => unknown;
}

const fileOf = (e: FileEntry) => new Promise<File>((ok, fail) => e.file(ok, fail));

/** A directory's children: the reader hands them out in batches until an empty one. */
async function childrenOf(dir: DirectoryEntry): Promise<Entry[]> {
  const reader = dir.createReader();
  const all: Entry[] = [];
  for (;;) {
    const batch = await new Promise<Entry[]>((ok, fail) => reader.readEntries(ok, fail));
    if (batch.length === 0) return all;
    all.push(...batch);
  }
}

async function walk(entry: Entry, base: string, out: DroppedFile[]): Promise<void> {
  const path = base === "" ? entry.name : `${base}/${entry.name}`;
  if (entry.isFile) {
    out.push({ path, file: await fileOf(entry as FileEntry) });
  } else if (entry.isDirectory) {
    for (const child of await childrenOf(entry as DirectoryEntry)) await walk(child, path, out);
  }
}

/**
 * The files of a drop. Every item's entry is taken before the first await: the browser empties
 * the `DataTransfer` once the event handler returns.
 */
export async function droppedFiles(items: ArrayLike<DropItem>): Promise<DroppedFile[]> {
  const taken: (Entry | File)[] = [];
  for (const item of Array.from(items)) {
    if (item.kind !== "file") continue;
    const entry = item.webkitGetAsEntry?.() as Entry | null | undefined;
    if (entry) taken.push(entry);
    else {
      const file = item.getAsFile();
      if (file) taken.push(file);
    }
  }
  const out: DroppedFile[] = [];
  for (const t of taken) {
    if (t instanceof File) out.push({ path: t.name, file: t });
    else await walk(t, "", out);
  }
  return out;
}

/** A file input's files: a folder input (`webkitdirectory`) gives each its relative path. */
export function inputFiles(files: ArrayLike<File>): DroppedFile[] {
  return Array.from(files, (file) => ({ path: file.webkitRelativePath || file.name, file }));
}

/** Where a dropped file goes: under the folder open (`prefix` is "" or ends with `/`). */
export function uploadKey(prefix: string, path: string): string {
  return prefix + path.split("/").filter(Boolean).join("/");
}
