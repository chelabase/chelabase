// Fetches ?url=… and reports whether it was reached (redirects are not followed).
export default {
  async fetch(request) {
    const target = new URL(request.url).searchParams.get("url");
    try {
      const res = await fetch(target, { redirect: "manual", signal: AbortSignal.timeout(5000) });
      return Response.json({ reached: true, status: res.status });
    } catch (e) {
      return Response.json({ reached: false, error: String(e && e.message) });
    }
  },
};
