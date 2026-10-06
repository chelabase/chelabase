import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

const loads = vi.hoisted(() => ({ count: 0 }));
vi.mock("@/components/api-reference", () => {
  loads.count += 1;
  return { default: () => <div data-testid="reference" /> };
});

import { ApiPage } from "./api-page";

describe("ApiPage", () => {
  it("api_page_is_lazy", async () => {
    // Importing the page must not load the reference component (Scalar and its CSS).
    expect(loads.count).toBe(0);
    render(<ApiPage />);
    expect(await screen.findByTestId("reference")).toBeInTheDocument();
    expect(loads.count).toBe(1);
  });
});
