// Reads the e2e table back through Neon's proxy with @neondatabase/serverless: once over
// WebSocket (Pool) and once over SQL over HTTP (neon()). The proxy's serverless side listens
// on CHELABASE_PROXY_HTTP_PORT (4444 by default), not 443, so both URLs point there.
// Env: DATABASE_URL, EXPECT (the row count), NODE_EXTRA_CA_CERTS (the proxy's CA),
// CHELABASE_PROXY_HTTP_PORT (default 4444), CHELABASE_PROXY_HOSTADDR (the proxy's address, when
// the endpoint host doesn't resolve to it).
import dns from "node:dns";
import { Pool, neon, neonConfig } from "@neondatabase/serverless";

// In the stack's runner the endpoint hosts (ep-<id>.…) don't resolve: they are served by the proxy,
// whose address is CHELABASE_PROXY_HOSTADDR. Dial that address for them; the TLS server name stays
// the endpoint host, so the certificate is still verified against it.
const proxyAddr = process.env.CHELABASE_PROXY_HOSTADDR;
if (proxyAddr) {
  const realLookup = dns.lookup;
  dns.lookup = (hostname, options, callback) => {
    if (typeof options === "function") {
      callback = options;
      options = {};
    }
    if (typeof options === "number") options = { family: options };
    if (!/^ep-[0-9a-f]+\./i.test(String(hostname))) return realLookup(hostname, options, callback);
    const family = proxyAddr.includes(":") ? 6 : 4;
    if (options?.all) callback(null, [{ address: proxyAddr, family }]);
    else callback(null, proxyAddr, family);
  };
}

const url = process.env.DATABASE_URL;
const expected = Number(process.env.EXPECT);
const httpPort = process.env.CHELABASE_PROXY_HTTP_PORT || "4444";

neonConfig.webSocketConstructor = WebSocket;
neonConfig.wsProxy = (host) => `${host}:${httpPort}/v2`;
neonConfig.fetchEndpoint = (host) => `https://${host}:${httpPort}/sql`;

const pool = new Pool({ connectionString: url });
const { rows } = await pool.query("SELECT count(*)::int AS n FROM e2e");
await pool.end();
if (rows[0].n !== expected) {
  throw new Error(`WebSocket: expected ${expected} rows, got ${rows[0].n}`);
}

const sql = neon(url);
const viaHttp = await sql`SELECT count(*)::int AS n FROM e2e`;
if (viaHttp[0].n !== expected) {
  throw new Error(`HTTP: expected ${expected} rows, got ${viaHttp[0].n}`);
}
console.log(`ok   @neondatabase/serverless read ${expected} rows over WebSocket and over HTTP`);
