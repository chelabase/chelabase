/** Studio's origin without trailing slashes. `raw` is the build-time `VITE_STUDIO_URL`. */
export function studioUrl(raw?: string): string {
  return (raw?.trim() || 'http://studio.chelabase.localhost:8000').replace(/\/+$/, '');
}
