import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

const loads = vi.hoisted(() => ({ count: 0 }));
vi.mock("@/components/api-reference", () => {
  loads.count += 1;
  return { default: ({ doc }: { doc: string }) => <div data-testid="reference" data-doc={doc} /> };
});

import { ApiPage } from "./api-page";

describe("ApiPage", () => {
  it("api_page_is_lazy", async () => {
    // Importing the page must not load the reference component (Scalar and its CSS).
    expect(loads.count).toBe(0);
    render(<ApiPage doc="management" onDocChange={() => {}} />);
    expect(await screen.findByTestId("reference")).toBeInTheDocument();
    expect(loads.count).toBe(1);
  });

  it("switches between the three API documents with tabs", async () => {
    const onDocChange = vi.fn();
    const { rerender } = render(<ApiPage doc="management" onDocChange={onDocChange} />);
    expect((await screen.findByTestId("reference")).dataset.doc).toBe("management");
    expect(screen.getAllByRole("tab").map((t) => t.textContent)).toEqual(["Management API", "Data API", "Auth API"]);
    fireEvent.click(screen.getByRole("tab", { name: "Data API" }));
    expect(onDocChange).toHaveBeenCalledWith("data");
    rerender(<ApiPage doc="data" onDocChange={onDocChange} />);
    expect(screen.getByTestId("reference").dataset.doc).toBe("data");
  });

  it("renders the reference as the current tab's panel", async () => {
    render(<ApiPage doc="auth" onDocChange={() => {}} />);
    const panel = screen.getByRole("tabpanel");
    expect((await within(panel).findByTestId("reference")).dataset.doc).toBe("auth");
  });
});
