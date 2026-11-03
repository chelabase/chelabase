import { describe, expect, it, vi } from "vitest";
import { loadEnv, readEnv } from "./env";

const full = {
  VITE_API_URL: "http://api.chelabase.localhost:8000",
  VITE_STUDIO_HOST: "studio.chelabase.localhost:8000",
};

describe("readEnv", () => {
  it("maps every VITE_ setting", () => {
    expect(readEnv(full)).toEqual({
      apiUrl: "http://api.chelabase.localhost:8000",
      studioHost: "studio.chelabase.localhost:8000",
    });
  });

  it("needs only the API URL; the host is the page's own", () => {
    expect(readEnv({ VITE_API_URL: "http://api.test" })).toEqual({ apiUrl: "http://api.test", studioHost: window.location.host });
  });

  it("names the missing API URL", () => {
    expect(() => readEnv({ VITE_API_URL: undefined })).toThrow(/VITE_API_URL/);
    expect(() => readEnv({ VITE_API_URL: "  " })).not.toThrow(/zitadel/i);
  });

  it("carries no identity-provider settings", () => {
    expect(Object.keys(readEnv(full)).sort()).toEqual(["apiUrl", "studioHost"]);
  });
});

describe("loadEnv", () => {
  const runtime = {
    apiUrl: "https://api.example.com",
    studioHost: "studio.example.com",
  };
  const answer = (status: number, body: unknown, type = "application/json") =>
    vi.fn(async (_url: string, _init?: RequestInit) =>
      new Response(typeof body === "string" ? body : JSON.stringify(body), { status, headers: { "content-type": type } }),
    );

  it("asks auth on Studio's own origin, for JSON", async () => {
    const fetchImpl = answer(200, runtime);
    await loadEnv(full, fetchImpl);
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toBe("/auth/v1/studio-config");
    expect(new Headers(init?.headers).get("accept")).toBe("application/json");
    expect(init?.credentials).toBe("omit");
  });

  it("uses the runtime config over the VITE_ build values", async () => {
    expect(await loadEnv(full, answer(200, runtime))).toEqual({
      apiUrl: "https://api.example.com",
      studioHost: "studio.example.com",
    });
  });

  it("falls back to the VITE_ values on a 404 (the workbench's Vite server)", async () => {
    expect(await loadEnv(full, answer(404, "Not Found", "text/plain"))).toEqual(readEnv(full));
    expect(await loadEnv(full, answer(404, { error: { code: "not_found", message: "x" } }))).toEqual(readEnv(full));
  });

  it("still names the missing VITE_ settings when a 404 has nothing to fall back on", async () => {
    await expect(loadEnv({}, answer(404, ""))).rejects.toThrow(/VITE_API_URL/);
  });

  it("says the instance is still setting up while auth answers 503, without falling back", async () => {
    await expect(loadEnv(full, answer(503, { error: { code: "unavailable", message: "x" } }))).rejects.toThrow(
      /still setting up/,
    );
  });

  it("refuses a config with a missing value or a non-JSON page", async () => {
    await expect(loadEnv(full, answer(200, { ...runtime, apiUrl: "" }))).rejects.toThrow(/apiUrl/);
    await expect(loadEnv(full, answer(200, { ...runtime, studioHost: 7 }))).rejects.toThrow(/studioHost/);
    await expect(loadEnv(full, answer(200, "<!doctype html>", "text/html"))).rejects.toThrow(/studio-config/);
  });

  it("reports an unreachable config instead of using the build values", async () => {
    const down = vi.fn(async () => {
      throw new TypeError("Failed to fetch");
    });
    await expect(loadEnv(full, down)).rejects.toThrow(/studio-config/);
  });
});
