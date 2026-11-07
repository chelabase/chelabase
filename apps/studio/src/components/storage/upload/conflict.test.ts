import { ApiError } from "@chelabase/management-client";
import { describe, expect, it } from "vitest";
import { keepBothName } from "./conflict";
import { runUpload } from "./runner";
import { fakeApi } from "./fake-api";

describe("keepBothName", () => {
  it("adds_the_number_before_the_extension_in_the_last_segment", () => {
    expect(keepBothName("c.txt", 1)).toBe("c (1).txt");
    expect(keepBothName("a/b/c.txt", 2)).toBe("a/b/c (2).txt");
    expect(keepBothName("a.b/README", 1)).toBe("a.b/README (1)");
    expect(keepBothName("photos/.env", 3)).toBe("photos/.env (3)");
    expect(keepBothName("archive.tar.gz", 1)).toBe("archive.tar (1).gz");
  });
});

describe("runUpload conflicts", () => {
  // #105 AC3
  it("keep_both_stores_the_new_file_under_a_free_suffixed_name_and_leaves_the_old_untouched", async () => {
    const api = fakeApi({ existing: { "docs/c.txt": "old", "docs/c (1).txt": "older" } });
    const out = await runUpload(api, { bucket: "b", key: "docs/c.txt", name: "docs/c.txt", file: new File(["new"], "c.txt"), choice: "keep-both", session: null }, api.hooks());
    expect(out.name).toBe("docs/c (2).txt");
    expect(await api.text("docs/c (2).txt")).toBe("new");
    expect(await api.text("docs/c.txt")).toBe("old");
    expect(await api.text("docs/c (1).txt")).toBe("older");
    expect(api.calls.every((c) => !c.upsert)).toBe(true);
  });

  it("without_a_choice_a_taken_name_is_a_conflict_and_replace_upserts", async () => {
    const api = fakeApi({ existing: { "c.txt": "old" } });
    const job = { bucket: "b", key: "c.txt", name: "c.txt", file: new File(["new"], "c.txt"), choice: null, session: null };
    const err = await runUpload(api, job, api.hooks()).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).code as string).toBe("conflict");
    expect(await api.text("c.txt")).toBe("old");
    expect((await runUpload(api, { ...job, choice: "replace" }, api.hooks())).name).toBe("c.txt");
    expect(await api.text("c.txt")).toBe("new");
    expect(api.calls.at(-1)?.upsert).toBe(true);
  });

  it("keep_both_also_probes_names_for_a_multipart_upload", async () => {
    const api = fakeApi({ existing: { "big.bin": "old" } });
    const file = new File([new Uint8Array(12)], "big.bin");
    const out = await runUpload(api, { bucket: "b", key: "big.bin", name: "big.bin", file, choice: "keep-both", session: null }, api.hooks(), { threshold: 8, partSize: 8 });
    expect(out.name).toBe("big (1).bin");
    expect(await api.text("big.bin")).toBe("old");
  });
});
