import { EditorView } from "@codemirror/view";
import type { Extension } from "@codemirror/state";

/**
 * The response's CSP nonce, for the `<style>` elements Studio creates at run time (#306). The
 * Studio image serves the shell as a template that writes a fresh nonce per response into
 * `<meta property="csp-nonce" nonce="…">` and names the same nonce in `style-src`
 * (infra/images/studio.Caddyfile). The browser hides a parsed `nonce` attribute, so it is read
 * from the element's `nonce` property.
 *
 * Undefined where nothing templated the shell (the Vite dev server, `vite preview`): the meta
 * still holds the literal template action, and those servers send no CSP.
 */
export function cspNonce(doc: Document = document): string | undefined {
  const meta = doc.querySelector<HTMLMetaElement>('meta[property="csp-nonce"]');
  if (!meta) return undefined;
  const value = (meta.nonce || meta.getAttribute("nonce") || "").trim();
  return /^[A-Za-z0-9+/=_-]+$/.test(value) ? value : undefined;
}

/** CodeMirror's style element takes the nonce too: its theme and base styles are a `<style>`. */
export function editorNonce(doc: Document = document): Extension {
  const nonce = cspNonce(doc);
  return nonce ? EditorView.cspNonce.of(nonce) : [];
}
