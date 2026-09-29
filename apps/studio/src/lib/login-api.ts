/** Studio's sign-in pages talk to auth through the gateway, with its HttpOnly cookies. */

export type LoginNext = { next: "done"; redirect: string } | { next: "verify" };

export class LoginApiError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "LoginApiError";
    this.status = status;
  }
}

export interface LoginApi {
  authRequest(authRequest: string): Promise<{ signUp: boolean }>;
  signIn(input: { authRequest: string; email: string; password: string }): Promise<LoginNext>;
  signUp(input: { authRequest: string; email: string; password: string; givenName: string; familyName: string }): Promise<LoginNext>;
  verifyEmail(code: string): Promise<LoginNext>;
  resendCode(): Promise<void>;
  requestPasswordReset(email: string): Promise<void>;
  confirmPasswordReset(input: { userId: string; code: string; password: string }): Promise<void>;
  logout(): Promise<void>;
}

const FALLBACK = "Sign-in is unavailable, try again";

export function createLoginApi(baseUrl: string, fetchImpl: typeof fetch = globalThis.fetch.bind(globalThis)): LoginApi {
  async function post<T>(path: string, body: unknown = {}): Promise<T> {
    let response: Response;
    try {
      response = await fetchImpl(`${baseUrl}/auth/v1/${path}`, {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json", Accept: "application/json" },
        body: JSON.stringify(body),
      });
    } catch {
      throw new LoginApiError(0, FALLBACK);
    }
    const text = await response.text().catch(() => "");
    let parsed: unknown = undefined;
    try {
      parsed = text ? JSON.parse(text) : undefined;
    } catch {
      parsed = undefined;
    }
    if (!response.ok) {
      const message = (parsed as { error?: unknown } | undefined)?.error;
      throw new LoginApiError(response.status, typeof message === "string" ? message : FALLBACK);
    }
    return parsed as T;
  }

  return {
    authRequest: (authRequest) => post("auth-request", { authRequest }),
    signIn: (input) => post("sign-in", input),
    signUp: (input) => post("sign-up", input),
    verifyEmail: (code) => post("verify-email", { code }),
    resendCode: async () => void (await post("resend-code")),
    requestPasswordReset: async (email) => void (await post("password-reset", { email })),
    confirmPasswordReset: async (input) => void (await post("password-reset/confirm", input)),
    logout: async () => void (await post("logout")),
  };
}
