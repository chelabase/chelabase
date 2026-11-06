/**
 * Search params stay plain strings. TanStack Router's default JSON-parses each value, which
 * turns all-digit ids and codes into (imprecise) numbers.
 */
export function parseSearch(search: string): Record<string, string> {
  return Object.fromEntries(new URLSearchParams(search));
}

export function stringifySearch(search: Record<string, unknown>): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(search)) {
    if (value !== undefined && value !== null && value !== "") params.set(key, String(value));
  }
  const text = params.toString();
  return text ? `?${text}` : "";
}
