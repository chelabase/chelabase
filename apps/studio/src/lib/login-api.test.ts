import { describe, expect, it, vi } from "vitest";
import { LoginApiError, createLoginApi } from "./login-api";

function fakeFetch(status: number, body: unknown) {
  return vi.fn().mockResolvedValue(new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }));
}

describe("createLoginApi", () => {
  it("posts JSON with credentials and returns the answer", async () => {
    const fetchImpl = fakeFetch(200, { next: "done", redirect: "http://localhost:3000/callback?code=c" });
    const api = createLoginApi("http://127.0.0.1:8000", fetchImpl);
    await expect(api.signIn({ authRequest: "V2_1", email: "a@b.c", password: "p" })).resolves.toEqual({
      next: "done",
      redirect: "http://localhost:3000/callback?code=c",
    });
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:8000/auth/v1/sign-in");
    expect(init).toMatchObject({ method: "POST", credentials: "include" });
    expect(init.headers["Content-Type"]).toBe("application/json");
    expect(JSON.parse(init.body)).toEqual({ authRequest: "V2_1", email: "a@b.c", password: "p" });
  });

  it("turns an error answer into LoginApiError with auth's message", async () => {
    const api = createLoginApi("http://x", fakeFetch(401, { error: "Email or password is incorrect" }));
    const err = await api.signIn({ authRequest: "V2_1", email: "a", password: "b" }).catch((e) => e);
    expect(err).toBeInstanceOf(LoginApiError);
    expect(err.status).toBe(401);
    expect(err.message).toBe("Email or password is incorrect");
  });

  it("sends an empty JSON object for body-less calls", async () => {
    const fetchImpl = fakeFetch(200, {});
    await createLoginApi("http://x", fetchImpl).logout();
    expect(fetchImpl.mock.calls[0][0]).toBe("http://x/auth/v1/logout");
    expect(fetchImpl.mock.calls[0][1].body).toBe("{}");
  });
});
