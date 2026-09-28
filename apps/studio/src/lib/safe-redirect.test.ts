import { describe, expect, it } from "vitest";
import { safeRedirect } from "./safe-redirect";

describe("safeRedirect", () => {
  it("allows Studio's own origin", () => {
    expect(safeRedirect("http://localhost:3000/", "http://localhost:3000")).toBe("http://localhost:3000/");
  });
  // Review Focus 4.
  it("logout_ignores_a_foreign_redirect", () => {
    for (const bad of ["https://evil.example/", "//evil.example", "javascript:alert(1)", "not a url", ""]) {
      expect(safeRedirect(bad, "http://localhost:3000")).toBe("/");
    }
  });
});
