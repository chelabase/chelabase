/** `target` only if it is an absolute URL on `origin`; otherwise Studio's home. */
export function safeRedirect(target: string, origin: string): string {
  try {
    const url = new URL(target);
    return url.origin === origin ? url.toString() : "/";
  } catch {
    return "/";
  }
}
