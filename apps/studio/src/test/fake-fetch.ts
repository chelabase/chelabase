export type FakeHandler = (request: Request) => Response | Promise<Response>;

export type FakeFetch = ((input: RequestInfo | URL, init?: RequestInit) => Promise<Response>) & {
  calls: Request[];
};

export function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

/** Routes are keyed "<METHOD> <pathname>"; origin and query string are ignored. */
export function fakeFetch(routes: Record<string, FakeHandler>): FakeFetch {
  const calls: Request[] = [];
  const fn = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const request = new Request(input, init);
    calls.push(request);
    const key = `${request.method} ${new URL(request.url).pathname}`;
    const handler = Object.hasOwn(routes, key) ? routes[key] : undefined;
    if (!handler) {
      return jsonResponse({ error: { code: "not_found", message: `no fake route for ${key}` } }, 404);
    }
    return handler(request);
  };
  return Object.assign(fn, { calls });
}

/** A handler that answers in order, then repeats the last answer (polling tests). */
export function sequence(...answers: FakeHandler[]): FakeHandler {
  let i = 0;
  return (request) => answers[Math.min(i++, answers.length - 1)](request);
}
