import { describe, expect, it } from "vitest";
import { safeRedirect } from "./safe-redirect";

describe("safeRedirect", () => {
  it("allows Studio's own origin", () => {
    expect(safeRedirect("http://studio.chelabase.localhost:8000/", "http://studio.chelabase.localhost:8000")).toBe("http://studio.chelabase.localhost:8000/");
  });
  // Review Focus 4.
  it("logout_ignores_a_foreign_redirect", () => {
    for (const bad of ["https://evil.example/", "//evil.example", "javascript:alert(1)", "not a url", ""]) {
      expect(safeRedirect(bad, "http://studio.chelabase.localhost:8000")).toBe("/");
    }
  });
});
