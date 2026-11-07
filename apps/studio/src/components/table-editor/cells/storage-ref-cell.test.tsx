import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { fakeApi } from "@/components/storage/upload/fake-api";
import { cellUploader } from "./storage-ref-upload";
import { StorageRefCell, StorageRefEditor, type StorageRefCellProps, type StorageRefEditorProps } from "./storage-ref-cell";
import type { RefState } from "./use-storage-refs";

const B = "3f2b8c1e-7d6a-4e59-9b0c-2a1d4e5f6a7b";
const URL_OF = (name: string) => `https://api.test/storage/v1/object/sign/avatars/${name}?token=t`;
const ok = (name: string, pub = false): RefState => ({ state: "ok", url: URL_OF(name), bucket: "avatars", name, public: pub });

function cell(p: Partial<StorageRefCellProps> = {}) {
  const props: StorageRefCellProps = {
    desc: { kind: "ref" },
    value: { bucket_id: B, name: "u/face.png" },
    resolved: ok("u/face.png"),
    canEdit: true,
    canSign: true,
    upload: vi.fn(),
    save: vi.fn(async () => null),
    onOpen: vi.fn(),
    ...p,
  };
  return { props, ...render(<StorageRefCell {...props} />) };
}

function editor(p: Partial<StorageRefEditorProps> = {}) {
  const props: StorageRefEditorProps = {
    open: true,
    onClose: vi.fn(),
    columnName: "photo",
    desc: { kind: "ref" },
    nullable: true,
    value: { bucket_id: B, name: "u/face.png" },
    resolved: ok("u/face.png"),
    canEdit: true,
    previewSource: null,
    buckets: [{ id: B, name: "avatars" }],
    pendingFile: null,
    upload: vi.fn(),
    save: vi.fn(async () => null),
    renderPicker: () => null,
    ...p,
  };
  return { props, ...render(<StorageRefEditor {...props} />) };
}

const dropOn = (el: Element, file: File) =>
  fireEvent.drop(el, { dataTransfer: { files: [file], types: ["Files"], items: [{ kind: "file", type: file.type, getAsFile: () => file }] } });

describe("StorageRefCell", () => {
  // #106 AC2: an image ref renders a thumbnail from its URL.
  it("an_image_ref_renders_a_thumbnail", () => {
    cell();
    const img = screen.getByRole("img", { name: "face.png" });
    expect(img).toHaveAttribute("src", URL_OF("u/face.png"));
  });

  // #106 AC2: another type renders a typed chip, no image.
  it("another_type_renders_a_typed_chip", () => {
    cell({ value: { bucket_id: B, name: "docs/report.pdf" }, resolved: ok("docs/report.pdf") });
    expect(screen.queryByRole("img")).toBeNull();
    expect(screen.getByText("report.pdf")).toBeInTheDocument();
    expect(screen.getByText("PDF")).toBeInTheDocument();
  });

  // A Viewer's signed URLs don't open: no thumbnail from them (preview goes through the download route).
  it("a_viewer_gets_a_chip_for_a_private_image_and_a_thumbnail_for_a_public_one", () => {
    const { unmount } = cell({ canSign: false, canEdit: false });
    expect(screen.queryByRole("img")).toBeNull();
    expect(screen.getByText("face.png")).toBeInTheDocument();
    unmount();
    cell({ canSign: false, canEdit: false, resolved: ok("u/face.png", true) });
    expect(screen.getByRole("img", { name: "face.png" })).toBeInTheDocument();
  });

  it("svg_never_renders_as_a_thumbnail", () => {
    cell({ value: { bucket_id: B, name: "logo.svg" }, resolved: ok("logo.svg") });
    expect(screen.queryByRole("img")).toBeNull();
    expect(screen.getByText("logo.svg")).toBeInTheDocument();
  });

  // #106 AC4: a ref the role can't read shows "no access", and the cell still opens for an editor.
  it("an_unreadable_ref_shows_no_access_and_stays_editable", async () => {
    const { props } = cell({ resolved: { state: "no_access" } });
    expect(screen.getByText("no access")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /no access/ }));
    expect(props.onOpen).toHaveBeenCalled();
  });

  // #106 AC5: a deleted target shows "missing".
  it("a_deleted_target_shows_missing", () => {
    cell({ resolved: { state: "missing" } });
    expect(screen.getByText("missing")).toBeInTheDocument();
  });

  it("null_shows_null", () => {
    cell({ value: null, resolved: undefined });
    expect(screen.getByText("NULL")).toBeInTheDocument();
  });

  it("a_value_that_names_no_ref_shows_its_text", () => {
    cell({ value: "garbage", resolved: undefined });
    expect(screen.getByText("garbage")).toBeInTheDocument();
  });

  // #106 AC3: a file dropped on a cell uploads with m14's runner; the new ref is saved for the row.
  it("a_dropped_file_uploads_and_the_row_saves_the_new_ref", async () => {
    const api = fakeApi({ existing: { "face.png": "old" } });
    const upload = cellUploader({ api, bucketIdOf: (b) => (b === "avatars" ? B : null), idOf: async () => null });
    const save = vi.fn(async () => null);
    const { container, rerender, props } = cell({ upload, save });
    dropOn(container.firstElementChild!, new File(["new"], "face.png", { type: "image/png" }));
    // The name was taken: Keep both, never an overwrite.
    await waitFor(() => expect(save).toHaveBeenCalledWith({ null: false, text: JSON.stringify({ bucket_id: B, name: "face (1).png" }) }));
    expect(await api.text("face (1).png")).toBe("new");
    expect(await api.text("face.png")).toBe("old");
    rerender(<StorageRefCell {...props} value={{ bucket_id: B, name: "face (1).png" }} resolved={ok("face (1).png")} />);
    expect(screen.getByRole("img", { name: "face (1).png" })).toBeInTheDocument();
  });

  it("a_drop_into_a_path_column_goes_to_its_bucket", async () => {
    const api = fakeApi();
    const upload = cellUploader({ api, bucketIdOf: () => null, idOf: async () => null });
    const save = vi.fn(async () => null);
    const { container } = cell({ desc: { kind: "path", bucket: "avatars" }, value: null, resolved: undefined, upload, save });
    dropOn(container.firstElementChild!, new File(["x"], "me.png", { type: "image/png" }));
    await waitFor(() => expect(save).toHaveBeenCalledWith({ null: false, text: "me.png" }));
  });

  it("a_drop_with_no_bucket_known_opens_the_editor_with_the_file", () => {
    const { container, props } = cell({ value: null, resolved: undefined });
    const file = new File(["x"], "me.png");
    dropOn(container.firstElementChild!, file);
    expect(props.onOpen).toHaveBeenCalledWith(file);
    expect(props.upload).not.toHaveBeenCalled();
  });

  it("a_save_error_shows_in_the_cell", async () => {
    const api = fakeApi();
    const upload = cellUploader({ api, bucketIdOf: () => B, idOf: async () => null });
    const { container } = cell({ upload, save: async () => "photo: permission denied" });
    dropOn(container.firstElementChild!, new File(["x"], "n.png"));
    expect(await screen.findByRole("alert")).toHaveTextContent("permission denied");
  });

  it("a_read_only_cell_takes_no_drop", () => {
    const { container, props } = cell({ canEdit: false });
    dropOn(container.firstElementChild!, new File(["x"], "n.png"));
    expect(props.upload).not.toHaveBeenCalled();
    expect(props.onOpen).not.toHaveBeenCalled();
  });
});

describe("StorageRefEditor", () => {
  // #106 AC6: Clear sets the column to NULL.
  it("clear_saves_null", async () => {
    const { props } = editor();
    await userEvent.click(screen.getByRole("button", { name: "Clear" }));
    expect(props.save).toHaveBeenCalledWith({ null: true });
    await waitFor(() => expect(props.onClose).toHaveBeenCalled());
  });

  // #106 AC6: on a NOT NULL column the value stays and the error shows.
  it("clear_on_a_not_null_column_keeps_the_value_and_shows_why", async () => {
    const { props } = editor({ nullable: false });
    await userEvent.click(screen.getByRole("button", { name: "Clear" }));
    expect(props.save).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent(/photo is NOT NULL/);
    expect(props.onClose).not.toHaveBeenCalled();
  });

  it("a_refused_clear_shows_the_error_and_stays_open", async () => {
    const { props } = editor({ save: vi.fn(async () => "photo: null value violates a check") });
    await userEvent.click(screen.getByRole("button", { name: "Clear" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("violates a check");
    expect(props.onClose).not.toHaveBeenCalled();
  });

  // #106 AC4: an owner (a writer) can still replace or clear a ref the role can't read.
  it("an_unreadable_ref_can_still_be_replaced_or_cleared", () => {
    editor({ resolved: { state: "no_access" } });
    expect(screen.getByText(/can't read this file/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Clear" })).toBeEnabled();
    expect(screen.getByLabelText("Upload a file")).toBeInTheDocument();
  });

  it("a_reader_sees_the_preview_but_no_actions", () => {
    editor({ canEdit: false });
    expect(screen.queryByRole("button", { name: "Clear" })).toBeNull();
    expect(screen.queryByLabelText("Upload a file")).toBeNull();
  });

  it("picking_an_object_saves_its_ref", async () => {
    const save = vi.fn(async () => null);
    editor({
      save,
      renderPicker: (bucket, onPick) => (
        <button type="button" onClick={() => onPick({ bucketId: B, bucket, name: "u/other.png", id: null })}>
          pick {bucket}
        </button>
      ),
    });
    await userEvent.click(screen.getByRole("button", { name: "pick avatars" }));
    expect(save).toHaveBeenCalledWith({ null: false, text: JSON.stringify({ bucket_id: B, name: "u/other.png" }) });
  });

  it("a_pending_dropped_file_uploads_to_the_chosen_bucket", async () => {
    const api = fakeApi();
    const upload = cellUploader({ api, bucketIdOf: () => B, idOf: async () => null });
    const save = vi.fn(async () => null);
    editor({ value: null, resolved: undefined, pendingFile: new File(["x"], "drop.png"), upload, save });
    await userEvent.click(screen.getByRole("button", { name: /Upload drop.png/ }));
    await waitFor(() => expect(save).toHaveBeenCalledWith({ null: false, text: JSON.stringify({ bucket_id: B, name: "drop.png" }) }));
  });
});
