/**
 * Studio's Zitadel redirect URI and the gateway's CORS origin are registered
 * for `localhost`, so a visit via 127.0.0.1 must move to localhost before
 * sign-in starts. Returns the URL to go to, or null to stay.
 */
export function canonicalRedirect(url: URL): string | null {
  if (url.hostname !== "127.0.0.1") return null;
  const moved = new URL(url.href);
  moved.hostname = "localhost";
  return moved.href;
}
