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

  it("surfaces the API's error message", async () => {
    const { api } = client(jsonResponse(409, { error: "database name already exists in project" }));
    await expect(
      api.createDatabase({ tenant_id: "t" as never, project_id: "p" as never, name: "x" }),
    ).rejects.toMatchObject({ status: 409, message: "database name already exists in project" });
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
