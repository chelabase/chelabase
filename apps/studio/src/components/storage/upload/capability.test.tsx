import { createEvent, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { DropZone } from "../browser/drop-zone";

function dataTransfer(names: string[]) {
  const files = names.map((n) => new File(["x"], n));
  return {
    types: ["Files"],
    files,
    items: files.map((f) => ({ kind: "file", getAsFile: () => f, webkitGetAsEntry: () => null })),
    dropEffect: "none",
  };
}

function drop(target: HTMLElement, names: string[]) {
  const dt = dataTransfer(names);
  fireEvent.dragEnter(target, { dataTransfer: dt });
  fireEvent.dragOver(target, { dataTransfer: dt });
  const event = createEvent.drop(target, { dataTransfer: dt });
  fireEvent(target, event);
  return { dt, event };
}

describe("DropZone", () => {
  // #105 AC7
  it("for_a_viewer_it_says_read_only_and_does_nothing", async () => {
    const onFiles = vi.fn();
    render(
      <DropZone canWrite={false} onFiles={onFiles}>
        <p>objects</p>
      </DropZone>,
    );
    const zone = screen.getByText("objects").parentElement!;
    const dt = dataTransfer(["a.txt"]);
    fireEvent.dragEnter(zone, { dataTransfer: dt });
    expect(await screen.findByText(/read-only/i)).toBeInTheDocument();
    const { event } = drop(zone, ["a.txt"]);
    expect(event.defaultPrevented).toBe(true);
    await new Promise((r) => setTimeout(r, 0));
    expect(onFiles).not.toHaveBeenCalled();
    // Paste does nothing either.
    fireEvent.paste(window, { clipboardData: { files: [new File(["x"], "p.png")] } });
    expect(onFiles).not.toHaveBeenCalled();
  });

  it("for_a_writer_a_drop_hands_over_the_files", async () => {
    const onFiles = vi.fn();
    render(
      <DropZone canWrite onFiles={onFiles}>
        <p>objects</p>
      </DropZone>,
    );
    const zone = screen.getByText("objects").parentElement!;
    fireEvent.dragEnter(zone, { dataTransfer: dataTransfer(["a.txt"]) });
    expect(await screen.findByText(/Drop to upload/i)).toBeInTheDocument();
    drop(zone, ["a.txt", "b.txt"]);
    await waitFor(() => expect(onFiles).toHaveBeenCalledTimes(1));
    expect(onFiles.mock.calls[0]![0].map((d: { path: string }) => d.path)).toEqual(["a.txt", "b.txt"]);
    expect(screen.queryByText(/Drop to upload/i)).not.toBeInTheDocument();
  });

  it("a_pasted_file_is_uploaded", async () => {
    const onFiles = vi.fn();
    render(
      <DropZone canWrite onFiles={onFiles}>
        <p>objects</p>
      </DropZone>,
    );
    fireEvent.paste(window, { clipboardData: { files: [new File(["x"], "p.png")] } });
    await waitFor(() => expect(onFiles).toHaveBeenCalledTimes(1));
    expect(onFiles.mock.calls[0]![0][0].path).toBe("p.png");
  });
});
