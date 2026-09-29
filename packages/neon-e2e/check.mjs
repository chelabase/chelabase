// Reads the e2e table back through Neon's proxy with @neondatabase/serverless: once over
// WebSocket (Pool) and once over SQL over HTTP (neon()). The proxy's serverless side listens
// on 4444, not 443, so both URLs point there.
// Env: DATABASE_URL, EXPECT (the row count), NODE_EXTRA_CA_CERTS (the proxy's CA).
import { Pool, neon, neonConfig } from "@neondatabase/serverless";

const url = process.env.DATABASE_URL;
const expected = Number(process.env.EXPECT);

neonConfig.webSocketConstructor = WebSocket;
neonConfig.wsProxy = (host) => `${host}:4444/v2`;
neonConfig.fetchEndpoint = (host) => `https://${host}:4444/sql`;

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
