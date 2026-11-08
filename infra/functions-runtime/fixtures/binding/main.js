// Calls the gateway through env.CHELABASE: ?path=… (default /data/v1/schema), with the request's
// own authorization, an optional ?host=… set as the Host header (which must not stick), and
// ?spoof=1 adding client-address headers (which must not reach the gateway).
// Reports the answer, or the error.
export default {
  async fetch(request, env) {
    const q = new URL(request.url).searchParams;
    const headers = { authorization: request.headers.get("authorization") ?? "" };
    if (q.get("host")) headers.host = q.get("host");
    if (q.get("spoof")) {
      // Client-address headers a member might set to pose as another caller (audit S1).
      headers["x-forwarded-for"] = "203.0.113.77";
      headers.forwarded = "for=203.0.113.78";
      headers["x-real-ip"] = "203.0.113.79";
      headers["x-forwarded-host"] = "studio.example";
      headers["x-forwarded-proto"] = "https";
      headers["true-client-ip"] = "203.0.113.80";
      headers["cf-connecting-ip"] = "203.0.113.81";
    }
    try {
      const target = q.get("url") ?? `https://anything.invalid${q.get("path") ?? "/data/v1/schema"}`;
      const res = await env.CHELABASE.fetch(target, { method: q.get("method") ?? "GET", headers });
      return Response.json({ status: res.status, body: await res.text(), bindings: Object.keys(env).sort() });
    } catch (e) {
      return Response.json({ error: String(e && e.message), bindings: Object.keys(env).sort() });
    }
  },
};
