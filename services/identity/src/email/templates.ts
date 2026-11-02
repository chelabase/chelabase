// Email templates: one plain-text and one HTML file per code kind under `templates/`. The
// text file's first line is `Subject: …`. Placeholders are `{{project}}`, `{{code}}`,
// `{{expires}}` and `{{link}}`; without a link, every line holding `{{link}}` is dropped.
import { readFileSync } from "node:fs";

export const TEMPLATE_KINDS = ["verify", "reset", "invite"] as const;
export type TemplateKind = (typeof TEMPLATE_KINDS)[number];

export interface TemplateValues {
  project: string;
  code: string;
  expires: string;
  link?: string;
}

export interface RenderedEmail {
  subject: string;
  text: string;
  html: string;
}

interface Template {
  subject: string;
  text: string;
  html: string;
}

const DIR = new URL("../../templates/", import.meta.url);

function load(kind: TemplateKind): Template {
  const raw = readFileSync(new URL(`${kind}.txt`, DIR), "utf8");
  const [first = "", ...rest] = raw.split("\n");
  if (!first.startsWith("Subject: ")) throw new Error(`templates/${kind}.txt must start with "Subject: "`);
  return {
    subject: first.slice("Subject: ".length).trim(),
    text: rest.join("\n").replace(/^\n+/, ""),
    html: readFileSync(new URL(`${kind}.html`, DIR), "utf8"),
  };
}

/** Read once at start-up: a missing template fails the process, not a request. */
const TEMPLATES = Object.fromEntries(TEMPLATE_KINDS.map((k) => [k, load(k)])) as Record<TemplateKind, Template>;

const escapeHtml = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

function fill(template: string, values: TemplateValues, escape: (s: string) => string): string {
  const lines = values.link === undefined ? template.split("\n").filter((l) => !l.includes("{{link}}")) : template.split("\n");
  const map: Record<string, string> = { project: values.project, code: values.code, expires: values.expires, link: values.link ?? "" };
  return lines.join("\n").replace(/\{\{(project|code|expires|link)\}\}/g, (_, key: string) => escape(map[key]!));
}

export function render(kind: TemplateKind, values: TemplateValues): RenderedEmail {
  const t = TEMPLATES[kind];
  return {
    subject: fill(t.subject, values, (s) => s),
    text: fill(t.text, values, (s) => s),
    html: fill(t.html, values, escapeHtml),
  };
}
