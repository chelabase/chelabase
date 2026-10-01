import { ApiError, type ErrorCode } from "@cheladb/management-client";
import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import { createStudioQueryClient, POLL_MS, pollWhile, TRANSITIONAL } from "./query";

function retryOf(client: QueryClient) {
  const r = client.getDefaultOptions().queries?.retry;
  if (typeof r !== "function") throw new Error("retry must be a function");
  return r as (count: number, err: Error) => boolean;
}

describe("createStudioQueryClient", () => {
  const client = createStudioQueryClient();
  const retry = retryOf(client);

  it("sets staleTime and refetchOnWindowFocus", () => {
    const q = client.getDefaultOptions().queries;
    expect(q?.staleTime).toBe(10_000);
    expect(q?.refetchOnWindowFocus).toBe(true);
  });

  it.each([
    ["400", new ApiError(400, "invalid_request", "x"), 0, false],
    ["404", new ApiError(404, "not_found", "x"), 0, false],
    ["499", new ApiError(499, "x" as ErrorCode, "x"), 0, false],
    ["503 first", new ApiError(503, "unavailable", "x"), 0, true],
    ["503 second", new ApiError(503, "unavailable", "x"), 1, true],
    ["503 third", new ApiError(503, "unavailable", "x"), 2, false],
    ["500 first", new ApiError(500, "internal", "x"), 0, true],
    ["network first", new ApiError(0, "network", "x"), 0, true],
    ["network second", new ApiError(0, "network", "x"), 1, true],
    ["network third", new ApiError(0, "network", "x"), 2, false],
    ["other ApiError", new ApiError(302, "weird" as ErrorCode, "x"), 0, false],
    ["plain Error", new Error("boom"), 0, false],
  ])("retry policy: %s", (_n, err, count, expected) => {
    expect(retry(count, err)).toBe(expected);
  });

  it("never retries mutations", () => {
    expect(client.getDefaultOptions().mutations?.retry).toBe(false);
  });
});

describe("pollWhile", () => {
  const fn = pollWhile<{ s: string }>((d) => d.s === "busy");
  const q = (data: { s: string } | undefined) => ({ state: { data } }) as never;

  it("polls while busy", () => expect(fn(q({ s: "busy" }))).toBe(POLL_MS));
  it("stops when settled", () => {
    expect(fn(q({ s: "ready" }))).toBe(false);
    expect(fn(q({ s: "failed" }))).toBe(false);
  });
  it("stops while sign-in is restarting", () => {
    const restarting = new ApiError(401, "unauthorized", "expired", {}, true);
    expect(fn({ state: { data: { s: "busy" }, error: restarting } } as never)).toBe(false);
    // Any other error keeps the last good data polling.
    const hiccup = new ApiError(502, "unavailable", "hiccup");
    expect(fn({ state: { data: { s: "busy" }, error: hiccup } } as never)).toBe(POLL_MS);
  });
  it("does not poll without data", () => expect(fn(q(undefined))).toBe(false));
  it("POLL_MS is 2000", () => expect(POLL_MS).toBe(2000));
});

describe("TRANSITIONAL", () => {
  it("lists the transitional statuses", () => {
    expect([...TRANSITIONAL].sort()).toEqual(["deleting", "pending", "provisioning", "resetting"]);
  });
});
