import { describe, expect, it } from "vitest";
import { canonicalRedirect } from "./origin";

describe("canonicalRedirect", () => {
  it("moves 127.0.0.1 to localhost, keeping port, path and query", () => {
    expect(canonicalRedirect(new URL("http://127.0.0.1:3000/app?x=1#h"))).toBe("http://localhost:3000/app?x=1#h");
  });

  it("leaves localhost alone", () => {
    expect(canonicalRedirect(new URL("http://localhost:3000/app"))).toBeNull();
  });

  it("leaves other hosts alone", () => {
    expect(canonicalRedirect(new URL("https://studio.example.com/"))).toBeNull();
  });
});
