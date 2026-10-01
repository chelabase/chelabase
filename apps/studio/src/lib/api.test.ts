import { describe, expect, it, vi } from "vitest";
import { ApiError, createApiClient } from "./api";

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function client(response: Response, token: string | undefined = "tok-1") {
  const fetchImpl = vi.fn().mockResolvedValue(response);
  const onUnauthorized = vi.fn();
  const api = createApiClient({ baseUrl: "http://gw", getToken: () => token, onUnauthorized, fetchImpl });
  return { api, fetchImpl, onUnauthorized };
}

describe("createApiClient", () => {
  it("sends the bearer token to the gateway", async () => {
    const { api, fetchImpl } = client(jsonResponse(200, { user: { sub: "u", org_id: "o" }, tenant: null }));
    const me = await api.me();
    expect(me.tenant).toBeNull();
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe("http://gw/v1/me");
    expect((init.headers as Record<string, string>).Authorization).toBe("Bearer tok-1");
  });

  it("posts create requests as JSON", async () => {
    const { api, fetchImpl } = client(jsonResponse(202, { job_id: "j", database_id: "d", status: "pending" }));
    await api.createDatabase({ tenant_id: "t" as never, project_id: "p" as never, name: "web1" });
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe("http://gw/v1/databases");
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body as string)).toEqual({ tenant_id: "t", project_id: "p", name: "web1" });
  });

  it("reveals connection details", async () => {
    const details = { host: "h", port: 5, database: "d", user: "postgres", password: "p", uri: "postgres://…", http_url: "https://h:4444/sql" };
    const { api, fetchImpl } = client(jsonResponse(200, details));
    await expect(api.getConnection("d1")).resolves.toEqual(details);
    expect(fetchImpl.mock.calls[0][0]).toBe("http://gw/v1/databases/d1/connection");
  });

  it("resets the password with a POST", async () => {
    const { api, fetchImpl } = client(jsonResponse(200, {}));
    await api.resetPassword("d1");
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe("http://gw/v1/databases/d1/reset-password");
    expect(init.method).toBe("POST");
  });

  it("deletes with DELETE and accepts 202", async () => {
    const { api, fetchImpl } = client(jsonResponse(202, { status: "deleting" }));
    await api.deleteDatabase("d1");
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe("http://gw/v1/databases/d1");
    expect(init.method).toBe("DELETE");
  });

  it("accepts an empty success body", async () => {
    const { api } = client(new Response(null, { status: 204 }));
    await expect(api.deleteDatabase("d1")).resolves.toBeUndefined();
  });

  it("surfaces the API's error message", async () => {
    const { api } = client(jsonResponse(409, { error: "database name already exists in project" }));
    await expect(
      api.createDatabase({ tenant_id: "t" as never, project_id: "p" as never, name: "x" }),
    ).rejects.toMatchObject({ status: 409, message: "database name already exists in project" });
  });

  it("surfaces the data API's nested error message", async () => {
    const { api } = client(jsonResponse(503, { error: { code: "unavailable", message: "the data API is starting" } }));
    await expect(
      api.createDatabase({ tenant_id: "t" as never, project_id: "p" as never, name: "x" }),
    ).rejects.toMatchObject({ status: 503, message: "the data API is starting" });
  });

  it("marks a 401 whose sign-in restart is under way", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(401, { error: "token expired" }));
    const api = createApiClient({ baseUrl: "http://gw", getToken: () => "t", onUnauthorized: () => true, fetchImpl });
    await expect(api.me()).rejects.toMatchObject({ status: 401, signInRestarted: true });
  });

  it("leaves a 401 unmarked when sign-in was not restarted", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(401, { error: "missing org claim" }));
    const api = createApiClient({ baseUrl: "http://gw", getToken: () => "t", onUnauthorized: () => false, fetchImpl });
    await expect(api.me()).rejects.toMatchObject({ status: 401, signInRestarted: false, message: "missing org claim" });
  });

  // Review Focus 5.
  it("restarts sign-in once on 401", async () => {
    const { api, onUnauthorized } = client(jsonResponse(401, { error: "token expired" }));
    await expect(api.me()).rejects.toBeInstanceOf(ApiError);
    expect(onUnauthorized).toHaveBeenCalledTimes(1);
  });
});

describe("branch calls", () => {
  const call = (fetchImpl: ReturnType<typeof vi.fn>) => {
    const [url, init] = fetchImpl.mock.calls[0];
    return { url, method: init.method, body: init.body ? JSON.parse(init.body as string) : undefined };
  };

  it("lists branches (a bare array)", async () => {
    const { api, fetchImpl } = client(jsonResponse(200, [{ id: "b1" }]));
    await expect(api.listBranches("d1")).resolves.toEqual([{ id: "b1" }]);
    expect(call(fetchImpl)).toMatchObject({ url: "http://gw/v1/databases/d1/branches", method: "GET" });
  });

  it("creates a branch with a parent and a timestamp", async () => {
    const { api, fetchImpl } = client(jsonResponse(202, { id: "b2" }));
    await api.createBranch("d1", { name: "dev", parent_branch_id: "p" as never, at: { timestamp: "2026-09-30T10:00:00.000Z" } });
    expect(call(fetchImpl)).toEqual({
      url: "http://gw/v1/databases/d1/branches",
      method: "POST",
      body: { name: "dev", parent_branch_id: "p", at: { timestamp: "2026-09-30T10:00:00.000Z" } },
    });
  });

  it("reveals a branch's connection and resets its password", async () => {
    const { api, fetchImpl } = client(jsonResponse(200, { host: "h" }));
    await api.getBranchConnection("d1", "b1");
    expect(call(fetchImpl)).toMatchObject({ url: "http://gw/v1/databases/d1/branches/b1/connection", method: "GET" });
    const second = client(jsonResponse(200, {}));
    await second.api.resetBranchPassword("d1", "b1");
    expect(call(second.fetchImpl)).toMatchObject({
      url: "http://gw/v1/databases/d1/branches/b1/reset-password",
      method: "POST",
    });
  });

  it("resets to parent, restores and deletes", async () => {
    const a = client(jsonResponse(202, { status: "resetting" }));
    await a.api.resetBranchToParent("d1", "b1");
    expect(call(a.fetchImpl)).toMatchObject({
      url: "http://gw/v1/databases/d1/branches/b1/reset-to-parent",
      method: "POST",
    });
    const b = client(jsonResponse(202, { status: "resetting", backup_branch_id: "bk", backup_name: "main-before-restore-x" }));
    await expect(b.api.restoreBranch("d1", "b1", { at: { timestamp: "2026-09-30T10:00:00.000Z" } })).resolves.toMatchObject({
      backup_branch_id: "bk",
    });
    expect(call(b.fetchImpl)).toEqual({
      url: "http://gw/v1/databases/d1/branches/b1/restore",
      method: "POST",
      body: { at: { timestamp: "2026-09-30T10:00:00.000Z" } },
    });
    const c = client(jsonResponse(202, { status: "deleting" }));
    await c.api.deleteBranch("d1", "b1");
    expect(call(c.fetchImpl)).toMatchObject({ url: "http://gw/v1/databases/d1/branches/b1", method: "DELETE" });
  });

  it("patches the database settings in seconds", async () => {
    const { api, fetchImpl } = client(jsonResponse(200, { history_window: 3600, max_branches: 3 }));
    await expect(api.updateDatabase("d1", { history_window: 3600, max_branches: 3 })).resolves.toEqual({
      history_window: 3600,
      max_branches: 3,
    });
    expect(call(fetchImpl)).toEqual({
      url: "http://gw/v1/databases/d1",
      method: "PATCH",
      body: { history_window: 3600, max_branches: 3 },
    });
  });

  it("lists, creates, rotates and revokes a branch's data API keys", async () => {
    const a = client(jsonResponse(200, [{ id: "k", kind: "public" }]));
    await expect(a.api.listKeys("d1", "b1")).resolves.toEqual([{ id: "k", kind: "public" }]);
    expect(call(a.fetchImpl)).toMatchObject({ url: "http://gw/v1/databases/d1/branches/b1/keys", method: "GET" });

    const b = client(jsonResponse(201, { id: "k2", key: "chk_sec_x" }));
    await expect(b.api.createKey("d1", "b1", { name: "ci", role: "authenticated" })).resolves.toMatchObject({ key: "chk_sec_x" });
    expect(call(b.fetchImpl)).toEqual({
      url: "http://gw/v1/databases/d1/branches/b1/keys",
      method: "POST",
      body: { name: "ci", role: "authenticated" },
    });

    const c = client(jsonResponse(201, { id: "k3", key: "chk_sec_y" }));
    await expect(c.api.rotateKey("d1", "b1", "k2")).resolves.toMatchObject({ id: "k3" });
    expect(call(c.fetchImpl)).toMatchObject({ url: "http://gw/v1/databases/d1/branches/b1/keys/k2/rotate", method: "POST" });

    const d = client(new Response(null, { status: 204 }));
    await expect(d.api.revokeKey("d1", "b1", "k2")).resolves.toBeUndefined();
    expect(call(d.fetchImpl)).toMatchObject({ url: "http://gw/v1/databases/d1/branches/b1/keys/k2", method: "DELETE" });
  });

  it("reads the data API schema of a branch", async () => {
    const { api, fetchImpl } = client(jsonResponse(200, { version: "1", default_schema: "public", schemas: {} }));
    await expect(api.getDataApiSchema("d1", "b1")).resolves.toMatchObject({ default_schema: "public" });
    expect(call(fetchImpl)).toMatchObject({ url: "http://gw/v1/databases/d1/branches/b1/data-api/schema", method: "GET" });
  });
});
