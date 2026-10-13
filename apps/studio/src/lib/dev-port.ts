// Studio's dev server port: 3000 unless CHELABASE_STUDIO_PORT says otherwise. Only the gateway
// reaches it (its CHELABASE_STUDIO_UPSTREAM, by default Studio's own loopback port); users open Studio's host.
export function devPort(env: Record<string, string | undefined>): number {
  const raw = env.CHELABASE_STUDIO_PORT;
  if (raw === undefined || raw.trim() === "") return 3000;
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`CHELABASE_STUDIO_PORT=${JSON.stringify(raw)} is not a port`);
  }
  return port;
}
