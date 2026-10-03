// Reads the e2e table back through Neon's proxy with @neondatabase/serverless: once over
// WebSocket (Pool) and once over SQL over HTTP (neon()). The proxy's serverless side listens
// on CHELADB_PROXY_HTTP_PORT (4444 by default), not 443, so both URLs point there.
// Env: DATABASE_URL, EXPECT (the row count), NODE_EXTRA_CA_CERTS (the proxy's CA),
// CHELADB_PROXY_HTTP_PORT (default 4444).
import { Pool, neon, neonConfig } from "@neondatabase/serverless";

const url = process.env.DATABASE_URL;
const expected = Number(process.env.EXPECT);
const httpPort = process.env.CHELADB_PROXY_HTTP_PORT || "4444";

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
