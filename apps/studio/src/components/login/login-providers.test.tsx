import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { EnvContext } from "@/lib/env-context";
import { configureLoginClient } from "@/lib/login-client/fetch";
import { useResendCode } from "@/lib/login-client/generated/endpoints";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { LoginProviders } from "./login-providers";

const env = { issuer: "i", clientId: "c", projectId: "p", orgId: "o", apiUrl: "http://gateway.test" };

function Probe() {
  const resend = useResendCode();
  return <button onClick={() => resend.mutate({ data: {} })}>{resend.isSuccess ? "sent" : "send"}</button>;
}

afterEach(() => vi.unstubAllGlobals());

describe("LoginProviders", () => {
  it("points the login client at the Studio env's API URL and provides a QueryClient", async () => {
    configureLoginClient({ baseUrl: "http://stale.test" });
    const f = fakeFetch({ "POST /auth/v1/resend-code": () => jsonResponse({}) });
    vi.stubGlobal("fetch", f);
    render(
      <EnvContext.Provider value={env}>
        <LoginProviders><Probe /></LoginProviders>
      </EnvContext.Provider>,
    );
    await userEvent.click(screen.getByRole("button", { name: "send" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "sent" })).toBeInTheDocument());
    expect(new URL(f.calls[0]!.url).origin).toBe("http://gateway.test");
    expect(f.calls[0]!.credentials).toBe("include");
  });
});
