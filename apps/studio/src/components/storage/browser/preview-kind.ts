/** How an object is previewed, from its stored content type. */
export type PreviewKind = "image" | "video" | "audio" | "pdf" | "text" | "none";

/** Text previews read at most this much (a ranged read); the rest is cut. */
export const TEXT_PREVIEW_BYTES = 2 * 1024 * 1024;

/** Images above this aren't loaded for a preview. */
export const IMAGE_PREVIEW_BYTES = 25 * 1024 * 1024;

const TOKEN = /^[a-z0-9!#$&^_.+-]+$/;

/**
 * The preview for a content type. Media storage serves inline (images but never SVG, video,
 * audio, PDF) load from a signed URL; text and JSON are read as text and shown as source, so
 * stored HTML is never rendered. Anything else, or a malformed type, has none.
 */
export function previewKind(contentType: string | null | undefined): PreviewKind {
  if (!contentType || contentType.includes(",")) return "none";
  const essence = contentType.split(";")[0].trim().toLowerCase();
  const slash = essence.indexOf("/");
  if (slash < 0) return "none";
  const type = essence.slice(0, slash);
  const sub = essence.slice(slash + 1);
  if (!TOKEN.test(type) || !TOKEN.test(sub)) return "none";
  if (sub.includes("svg") || sub.includes("xhtml")) return "none";
  switch (type) {
    case "image":
      return sub.includes("xml") ? "none" : "image";
    case "video":
      return "video";
    case "audio":
      return "audio";
    case "text":
      return "text";
    case "application":
      if (sub === "pdf") return "pdf";
      if (sub === "json" || sub.endsWith("+json") || sub === "x-ndjson") return "text";
      return "none";
    default:
      return "none";
  }
}

/** JSON indented for reading; anything that doesn't parse stays as it is. */
export function prettyText(text: string, contentType: string | null | undefined): string {
  const essence = (contentType ?? "").split(";")[0].trim().toLowerCase();
  if (essence !== "application/json" && !essence.endsWith("+json")) return text;
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}
