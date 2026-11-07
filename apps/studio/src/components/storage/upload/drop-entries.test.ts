import { describe, expect, it } from "vitest";
import { droppedFiles, inputFiles, uploadKey } from "./drop-entries";

/** A fake of the File and Directory Entries API, as `webkitGetAsEntry` hands it out. */
type Tree = { [name: string]: Tree | string };

function fileEntry(name: string, fullPath: string, text: string) {
  return { isFile: true, isDirectory: false, name, fullPath, file: (ok: (f: File) => void) => ok(new File([text], name)) };
}

/** A directory whose reader hands out at most `batch` entries per call, as Chrome does (100). */
function dirEntry(name: string, fullPath: string, tree: Tree, batch: number): unknown {
  const children = Object.entries(tree).map(([n, v]) =>
    typeof v === "string" ? fileEntry(n, `${fullPath}/${n}`, v) : dirEntry(n, `${fullPath}/${n}`, v, batch),
  );
  return {
    isFile: false,
    isDirectory: true,
    name,
    fullPath,
    createReader: () => {
      let at = 0;
      return {
        readEntries: (ok: (e: unknown[]) => void) => {
          const out = children.slice(at, at + batch);
          at += out.length;
          // Asynchronous, as browsers answer.
          queueMicrotask(() => ok(out));
        },
      };
    },
  };
}

function items(tree: Tree, batch = 100) {
  return Object.entries(tree).map(([n, v]) => {
    const entry = typeof v === "string" ? fileEntry(n, `/${n}`, v) : dirEntry(n, `/${n}`, v, batch);
    return { kind: "file", webkitGetAsEntry: () => entry, getAsFile: () => (typeof v === "string" ? new File([v], n) : null) };
  });
}

const paths = async (p: Promise<{ path: string; file: File }[]>) => (await p).map((d) => d.path).sort();

describe("droppedFiles", () => {
  // #105 AC1
  it("a_dropped_folder_keeps_its_relative_paths", async () => {
    const dropped = await droppedFiles(items({ a: { b: { "c.txt": "hello" } }, "top.png": "x" }));
    expect(dropped.map((d) => d.path).sort()).toEqual(["a/b/c.txt", "top.png"]);
    const c = dropped.find((d) => d.path === "a/b/c.txt");
    expect(await c?.file.text()).toBe("hello");
  });

  it("a_folder_read_in_batches_returns_every_entry", async () => {
    const many: Tree = {};
    for (let i = 0; i < 250; i++) many[`f${i}.txt`] = "";
    const got = await paths(droppedFiles(items({ big: many }, 100)));
    expect(got).toHaveLength(250);
    expect(got).toContain("big/f249.txt");
  });

  it("empty_folders_add_nothing_and_non_file_items_are_ignored", async () => {
    const list = [...items({ empty: {}, "a.txt": "" }), { kind: "string", webkitGetAsEntry: () => null, getAsFile: () => null }];
    expect(await paths(droppedFiles(list))).toEqual(["a.txt"]);
  });

  it("an_item_without_entries_falls_back_to_its_file", async () => {
    const list = [{ kind: "file", getAsFile: () => new File(["x"], "pasted.png") }];
    expect(await paths(droppedFiles(list))).toEqual(["pasted.png"]);
  });
});

describe("inputFiles", () => {
  it("a_folder_input_uses_webkit_relative_path_and_a_file_input_the_name", () => {
    const inFolder = new File(["x"], "c.txt");
    Object.defineProperty(inFolder, "webkitRelativePath", { value: "a/b/c.txt" });
    expect(inputFiles([inFolder, new File(["y"], "d.txt")]).map((d) => d.path)).toEqual(["a/b/c.txt", "d.txt"]);
  });
});

describe("uploadKey", () => {
  // #105 AC1: under the current folder.
  it("a_relative_path_lands_under_the_current_folder", () => {
    expect(uploadKey("", "a/b/c.txt")).toBe("a/b/c.txt");
    expect(uploadKey("photos/2024/", "a/b/c.txt")).toBe("photos/2024/a/b/c.txt");
    expect(uploadKey("photos/", "/a//b/c.txt")).toBe("photos/a/b/c.txt");
  });
});
