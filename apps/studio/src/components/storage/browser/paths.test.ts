import { describe, expect, it } from "vitest";
import { baseName, crumbsFor, folderHref, folderPathOf, joinKey, nameProblem, prefixOf } from "./paths";

describe("folder paths", () => {
  it("a_folder_path_lists_under_its_prefix", () => {
    expect(prefixOf(undefined)).toBe("");
    expect(prefixOf("")).toBe("");
    expect(prefixOf("photos")).toBe("photos/");
    expect(prefixOf("photos/2024/")).toBe("photos/2024/");
  });

  it("a_folder_item_opens_its_own_path", () => {
    expect(folderPathOf("photos/2024/")).toBe("photos/2024");
    expect(folderPathOf("photos/")).toBe("photos");
  });

  it("names_show_without_the_folder_they_are_in", () => {
    expect(baseName("photos/2024/a.png")).toBe("a.png");
    expect(baseName("photos/2024/")).toBe("2024");
    expect(baseName("a.png")).toBe("a.png");
  });

  // #104 AC1: the breadcrumb matches the URL, each crumb naming the folder it opens.
  it("crumbs_name_the_bucket_then_each_folder", () => {
    expect(crumbsFor("avatars", undefined)).toEqual([{ label: "avatars", path: undefined }]);
    expect(crumbsFor("avatars", "photos/2024")).toEqual([
      { label: "avatars", path: undefined },
      { label: "photos", path: "photos" },
      { label: "2024", path: "photos/2024" },
    ]);
  });

  it("folder_links_encode_each_segment", () => {
    expect(folderHref("/app/storage/p1/d1/b1/avatars", "my photos/2024")).toBe("/app/storage/p1/d1/b1/avatars/my%20photos/2024");
    expect(folderHref("/app/storage/p1/d1/b1/avatars", undefined)).toBe("/app/storage/p1/d1/b1/avatars");
  });

  it("a_key_joins_a_folder_and_a_name", () => {
    expect(joinKey("", "a.png")).toBe("a.png");
    expect(joinKey("photos", "a.png")).toBe("photos/a.png");
    expect(joinKey("photos/", "a.png")).toBe("photos/a.png");
    expect(joinKey("/photos/", "a.png")).toBe("photos/a.png");
  });
});

describe("nameProblem", () => {
  it("accepts_a_plain_name", () => {
    expect(nameProblem("report.pdf")).toBeNull();
  });
  it("refuses_empty_slash_and_dot_names", () => {
    expect(nameProblem("")).toMatch(/name/i);
    expect(nameProblem("  ")).toMatch(/name/i);
    expect(nameProblem("a/b")).toMatch(/\//);
    expect(nameProblem(".")).toMatch(/\./);
    expect(nameProblem("..")).toMatch(/\./);
    expect(nameProblem("a\\b")).toMatch(/\\/);
  });
});
