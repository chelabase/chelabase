import { render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ObjectPreview } from "./object-preview";
import { TEXT_PREVIEW_BYTES, previewKind } from "./preview-kind";

describe("previewKind", () => {
  it("names_the_media_storage_serves_inline", () => {
    expect(previewKind("image/png")).toBe("image");
    expect(previewKind("image/webp; q=1")).toBe("image");
    expect(previewKind("video/mp4")).toBe("video");
    expect(previewKind("audio/mpeg")).toBe("audio");
    expect(previewKind("application/pdf")).toBe("pdf");
  });

  it("reads_text_and_json_as_text", () => {
    expect(previewKind("text/plain; charset=utf-8")).toBe("text");
    expect(previewKind("text/csv")).toBe("text");
    expect(previewKind("application/json")).toBe("text");
    expect(previewKind("application/geo+json")).toBe("text");
  });

  // Stored HTML is only ever shown as its source; SVG (script) is never rendered.
  it("never_renders_html_or_svg", () => {
    expect(previewKind("text/html")).toBe("text");
    expect(previewKind("image/svg+xml")).toBe("none");
    expect(previewKind("application/xhtml+xml")).toBe("none");
  });

  it("unknown_or_malformed_types_have_no_preview", () => {
    expect(previewKind("application/octet-stream")).toBe("none");
    expect(previewKind(null)).toBe("none");
    expect(previewKind("garbage")).toBe("none");
    expect(previewKind("image/png, text/html")).toBe("none");
  });
});

const SIGNED = "https://api.test/storage/v1/object/sign/avatars/a.png?token=t";

afterEach(() => vi.unstubAllGlobals());

describe("ObjectPreview", () => {
  // #104 AC5: an image renders from its signed URL.
  it("an_image_renders_from_the_signed_url", async () => {
    const sign = vi.fn(async () => SIGNED);
    render(<ObjectPreview name="a.png" contentType="image/png" size={1000} sign={sign} />);
    const img = await screen.findByRole("img", { name: "a.png" });
    expect(img).toHaveAttribute("src", SIGNED);
    expect(sign).toHaveBeenCalledTimes(1);
  });

  // #104 AC5: an unknown type gets no preview and nothing is signed for it.
  it("an_unknown_type_shows_no_preview", () => {
    const sign = vi.fn(async () => SIGNED);
    render(<ObjectPreview name="blob.bin" contentType="application/octet-stream" size={10} sign={sign} />);
    expect(screen.getByText(/No preview for this type/)).toBeInTheDocument();
    expect(screen.queryByRole("img")).toBeNull();
    expect(sign).not.toHaveBeenCalled();
  });

  it("without_signing_it_says_previews_need_a_signed_url", () => {
    render(<ObjectPreview name="a.png" contentType="image/png" size={10} sign={null} />);
    expect(screen.getByText(/needs Developer access/)).toBeInTheDocument();
    expect(screen.queryByRole("img")).toBeNull();
  });

  it("text_is_read_as_text_with_at_most_2_mib_asked_for", async () => {
    const fetchMock = vi.fn(async () => new Response("<b>hi</b>", { status: 206 }));
    vi.stubGlobal("fetch", fetchMock);
    const { container } = render(<ObjectPreview name="page.html" contentType="text/html" size={TEXT_PREVIEW_BYTES + 10} sign={async () => SIGNED} />);
    expect(await screen.findByText("<b>hi</b>")).toBeInTheDocument();
    // Shown as source: no element from the file is created.
    expect(container.querySelector("b")).toBeNull();
    const init = (fetchMock.mock.calls[0] as unknown as [string, RequestInit])[1];
    expect(new Headers(init.headers).get("Range")).toBe(`bytes=0-${TEXT_PREVIEW_BYTES - 1}`);
    expect(screen.getByText(/first 2 MiB/)).toBeInTheDocument();
  });

  it("json_is_pretty_printed", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response('{"a":1}')));
    render(<ObjectPreview name="x.json" contentType="application/json" size={7} sign={async () => SIGNED} />);
    await waitFor(() => expect(screen.getByTestId("text-preview").textContent).toBe('{\n  "a": 1\n}'));
  });

  it("a_pdf_is_framed_from_the_signed_url", async () => {
    render(<ObjectPreview name="r.pdf" contentType="application/pdf" size={10} sign={async () => SIGNED} />);
    expect(await screen.findByTitle("r.pdf")).toHaveAttribute("src", SIGNED);
  });

  it("a_signing_failure_is_shown", async () => {
    render(<ObjectPreview name="a.png" contentType="image/png" size={10} sign={async () => Promise.reject(new Error("object not found"))} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("object not found");
  });
});
