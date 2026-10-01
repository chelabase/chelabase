// A fake `fetch` for the tests: answers every request with `body` and records the calls.

export interface Recorded {
  url: string;
  init: RequestInit;
}

export function fakeFetch(
  body: string,
  status = 200,
  headers: Record<string, string> = { "content-type": "application/json" },
): typeof fetch & { calls: Recorded[] } {
  const calls: Recorded[] = [];
  const f = async (input: string | URL | Request, init?: RequestInit) => {
    calls.push({ url: String(input), init: init ?? {} });
    return new Response(body, { status, headers });
  };
  return Object.assign(f as typeof fetch, { calls });
}
