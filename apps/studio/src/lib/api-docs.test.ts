import { describe, expect, it } from "vitest";
import { API_DOCS, appUserAuthDocument, parseApiSearch } from "./api-docs";

describe("api docs", () => {
  it("lists Management, Data and Auth in that order", () => {
    expect(API_DOCS.map((d) => d.label)).toEqual(["Management API", "Data API", "Auth API"]);
  });

  it("parses ?doc, falling back to Management for anything unknown", () => {
    expect(parseApiSearch({ doc: "data" })).toEqual({ doc: "data" });
    expect(parseApiSearch({ doc: "auth" })).toEqual({ doc: "auth" });
    expect(parseApiSearch({ doc: "login" })).toEqual({});
    expect(parseApiSearch({ doc: 3 })).toEqual({});
    expect(parseApiSearch({})).toEqual({});
  });

  it("keeps only the app-user endpoints in the Auth document", () => {
    const doc = appUserAuthDocument();
    const paths = Object.keys(doc.paths);
    expect(paths.length).toBeGreaterThan(0);
    expect(paths.every((p) => p.startsWith("/auth/v1/app/"))).toBe(true);
    expect(paths).toContain("/auth/v1/app/sign-in");
    // Studio's own sign-in endpoints are not a public contract.
    expect(paths).not.toContain("/auth/v1/sign-in");
    expect(paths).not.toContain("/auth/v1/studio-config");
    expect(doc.info.title).toBe("Chelabase Auth API");
  });
});
