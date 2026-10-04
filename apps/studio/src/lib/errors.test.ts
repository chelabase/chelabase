import { ApiError } from "@chelabase/management-client";
import { describe, expect, it } from "vitest";
import { errorText } from "./errors";

describe("errorText", () => {
  it("uses the ApiError message", () => {
    expect(errorText(new ApiError(409, "name_taken", "Name taken"))).toBe("Name taken");
  });
  it("rewrites network errors", () => {
    expect(errorText(new ApiError(0, "network", "ignored"))).toBe("Can't reach Chelabase; check your connection");
  });
  it("uses a plain Error message", () => {
    expect(errorText(new Error("boom"))).toBe("boom");
  });
  it("falls back for non-errors", () => {
    expect(errorText("x")).toBe("Something went wrong");
    expect(errorText(undefined)).toBe("Something went wrong");
  });
});
