import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, configureManagementClient } from "@chelabase/management-client";
import { ALL_DATABASES, downloadTypes, typesRequest } from "./types-download";

function serve(body: string, status = 200, type = "text/typescript; charset=utf-8") {
  const f = vi.fn(async () => new Response(body, { status, headers: { "content-type": type } }));
  configureManagementClient({ baseUrl: "https://api.test", getToken: () => "tok", onUnauthorized: () => {}, fetchImpl: f as unknown as typeof fetch });
  return f;
}

afterEach(() => vi.restoreAllMocks());

describe("typesRequest", () => {
  it("asks for main's module as the role, saved as db.ts", () => {
    expect(typesRequest({ role: " authenticated ", database: null })).toEqual({
      params: { role: "authenticated" },
      filename: "db.ts",
    });
  });

  it("names another database, or every database", () => {
    expect(typesRequest({ role: "reporting", database: "analytics" }).params).toEqual({ role: "reporting", database: "analytics" });
    expect(typesRequest({ role: "reporting", database: ALL_DATABASES }).params).toEqual({ role: "reporting", all_databases: true });
  });
});

describe("downloadTypes", () => {
  it("sends the request with the bearer and saves the module as db.ts", async () => {
    const module = "export type Database = {};\n";
    const f = serve(module);
    const save = vi.fn();
    await downloadTypes("d1", "b1", { role: "authenticated", database: "analytics" }, save);
    const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://api.test/v1/databases/d1/branches/b1/data-api/types?role=authenticated&database=analytics");
    expect(init.method).toBe("GET");
    expect(new Headers(init.headers).get("authorization")).toBe("Bearer tok");
    expect(save).toHaveBeenCalledWith(module, "db.ts");
  });

  it("every database is one request with all_databases", async () => {
    const f = serve("export type Databases = {};\n");
    const save = vi.fn();
    await downloadTypes("d1", "b1", { role: "authenticated", database: ALL_DATABASES }, save);
    const [url] = f.mock.calls[0] as unknown as [string];
    expect(url).toBe("https://api.test/v1/databases/d1/branches/b1/data-api/types?role=authenticated&all_databases=true");
    expect(save).toHaveBeenCalledWith("export type Databases = {};\n", "db.ts");
  });

  it("a refusal throws the API error and saves nothing", async () => {
    serve('{"error":{"code":"forbidden","message":"this needs role:x","required":"role:x"}}', 403, "application/json");
    const save = vi.fn();
    await expect(downloadTypes("d1", "b1", { role: "x", database: null }, save)).rejects.toBeInstanceOf(ApiError);
    expect(save).not.toHaveBeenCalled();
  });

  it("an empty role is refused before any request", async () => {
    const f = serve("");
    await expect(downloadTypes("d1", "b1", { role: "  ", database: null }, vi.fn())).rejects.toThrow("Enter a role");
    expect(f).not.toHaveBeenCalled();
  });
});
