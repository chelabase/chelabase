// Studio's dev server port. A worktree slot N runs it on 3000 + 100N (`just dev` sets
// CHELADB_STUDIO_PORT); Zitadel has every slot's port registered as a redirect URI.
export function devPort(env: Record<string, string | undefined>): number {
  const raw = env.CHELADB_STUDIO_PORT;
  if (raw === undefined || raw.trim() === "") return 3000;
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`CHELADB_STUDIO_PORT=${JSON.stringify(raw)} is not a port`);
  }
  return port;
}
