// Calls the gateway through env.CHELABASE: ?path=… (default /data/v1/schema), with the request's
// own authorization and an optional ?host=… set as the Host header (which must not stick).
// Reports the answer, or the error.
export default {
  async fetch(request, env) {
    const q = new URL(request.url).searchParams;
    const headers = { authorization: request.headers.get("authorization") ?? "" };
    if (q.get("host")) headers.host = q.get("host");
    try {
      const target = q.get("url") ?? `https://anything.invalid${q.get("path") ?? "/data/v1/schema"}`;
      const res = await env.CHELABASE.fetch(target, { method: q.get("method") ?? "GET", headers });
      return Response.json({ status: res.status, body: await res.text(), bindings: Object.keys(env).sort() });
    } catch (e) {
      return Response.json({ error: String(e && e.message), bindings: Object.keys(env).sort() });
    }
  },
};
