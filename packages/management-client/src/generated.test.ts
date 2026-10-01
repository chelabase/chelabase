import { describe, expect, it } from "vitest";
import { configureManagementClient, getDatabase } from "./index";

describe("generated client", () => {
  it("generated_functions_return_success_bodies", async () => {
    const calls: { url: string; method: string | undefined }[] = [];
    const fetchImpl = (async (url: string, init?: RequestInit) => {
      calls.push({ url, method: init?.method });
      return new Response(JSON.stringify({ id: "x" }), { status: 200 });
    }) as unknown as typeof fetch;
    configureManagementClient({ baseUrl: "http://api.test", getToken: () => undefined, onUnauthorized: () => false, fetchImpl });
    const id = "6f1c5d1e-0000-4000-8000-000000000001";
    const body = await getDatabase(id);
    expect(calls).toHaveLength(1);
    expect(calls[0]!.url).toBe(`http://api.test/v1/databases/${id}`);
    expect(calls[0]!.method).toBe("GET");
    expect(body).toEqual({ id: "x" });
  });
});
