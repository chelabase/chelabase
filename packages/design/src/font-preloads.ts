// Preloads for the web fonts a first screen renders, as the hashed URLs Vite emits. Imported here
// so only this package names the font packages (D8). The stylesheet (tokens.css) references the
// same files, so Vite emits each once and the preload is the file the page uses. Only the latin
// subsets, and only faces apps show above the fold: Spectral 600 and 700 (headings, the wordmark)
// and Atkinson (body text and buttons at every weight; one variable file).
import atkinsonLatin from "@fontsource-variable/atkinson-hyperlegible-next/files/atkinson-hyperlegible-next-latin-wght-normal.woff2?url";
import spectralLatin600 from "@fontsource/spectral/files/spectral-latin-600-normal.woff2?url";
import spectralLatin700 from "@fontsource/spectral/files/spectral-latin-700-normal.woff2?url";

const preload = (href: string) => ({
  rel: "preload",
  href,
  as: "font",
  type: "font/woff2",
  crossOrigin: "anonymous" as const,
});

/** `<link rel="preload">` descriptors for a TanStack route's `head().links`, one per face. Each
 *  app preloads only the faces its first screen shows. */
export const fontPreload = {
  spectral600: preload(spectralLatin600),
  spectral700: preload(spectralLatin700),
  atkinson: preload(atkinsonLatin),
};
