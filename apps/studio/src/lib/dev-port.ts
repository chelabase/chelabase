// Studio's dev server port: 3000 unless CHELABASE_STUDIO_PORT says otherwise. Zitadel's Studio
// app has http://localhost:3000 registered as its redirect URI.
export function devPort(env: Record<string, string | undefined>): number {
  const raw = env.CHELABASE_STUDIO_PORT;
  if (raw === undefined || raw.trim() === "") return 3000;
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`CHELABASE_STUDIO_PORT=${JSON.stringify(raw)} is not a port`);
  }
  return port;
}
