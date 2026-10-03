// CodeMirror wiring for the SQL editor: PostgreSQL highlighting always, and pgls completion,
// lint and hover when a client is given (else lang-sql's keyword completion, "Basic completion").
// Only types come from client.ts, so this module never pulls in the worker or the wasm.
import {
  autocompletion,
  type Completion,
  type CompletionContext,
  type CompletionResult,
  type CompletionSource,
} from "@codemirror/autocomplete";
import { PostgreSQL, sql } from "@codemirror/lang-sql";
import { LanguageSupport } from "@codemirror/language";
import { type Diagnostic, linter, type LintSource } from "@codemirror/lint";
import type { EditorState, Extension } from "@codemirror/state";
import { hoverTooltip } from "@codemirror/view";
import type { PglsClient, PglsCompletion } from "./client";

const LINT_DELAY_MS = 300;

const COMPLETION_TYPES: Record<PglsCompletion["kind"], string> = {
  table: "class",
  column: "property",
  function: "function",
  schema: "namespace",
  keyword: "keyword",
  policy: "variable",
  role: "variable",
};

const WORD = /[\w$]*/;

/** Items from the system catalogs rank below the user's own (a prefix match still finds them). */
const SYSTEM_BOOST = -50;
const SYSTEM_SCHEMA = /\b(pg_catalog|information_schema|pg_toast)\b/;

function isSystem(item: PglsCompletion): boolean {
  return SYSTEM_SCHEMA.test(item.detail ?? "") || (item.kind === "schema" && SYSTEM_SCHEMA.test(item.label));
}

/**
 * pgls's hover markdown as plain text: code fences dropped, heading markers and backticks
 * stripped, line breaks kept. The caller sets it as text, so nothing in it is parsed as HTML.
 */
export function hoverText(markdown: string): string {
  return markdown
    .split("\n")
    .filter((line) => !line.trimStart().startsWith("```"))
    .map((line) => line.replace(/^#{1,6}\s+/, "").replaceAll("`", ""))
    .join("\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

/** Completes the word at the cursor (after a `.`, from just past the dot) with pgls's items. */
export function pglsCompletionSource(client: PglsClient): CompletionSource {
  return async (ctx: CompletionContext): Promise<CompletionResult | null> => {
    const word = ctx.matchBefore(WORD);
    const from = word ? word.from : ctx.pos;
    const afterDot = from > 0 && ctx.state.sliceDoc(from - 1, from) === ".";
    if (!ctx.explicit && from === ctx.pos && !afterDot) return null;
    let items: PglsCompletion[];
    try {
      await client.update(ctx.state.doc.toString());
      items = await client.complete(ctx.pos);
    } catch {
      return null;
    }
    if (ctx.aborted) return null;
    return {
      from,
      to: ctx.pos,
      options: items.map(
        (item): Completion => ({
          label: item.label,
          type: COMPLETION_TYPES[item.kind] ?? "variable",
          detail: item.detail,
          ...(isSystem(item) ? { boost: SYSTEM_BOOST } : {}),
        }),
      ),
      validFor: /^[\w$]*$/,
    };
  };
}

/** pgls diagnostics for the current document (`hint` shown as `info`). */
export function pglsLintSource(client: PglsClient): LintSource {
  return async (view: { state: EditorState }): Promise<Diagnostic[]> => {
    const length = view.state.doc.length;
    try {
      await client.update(view.state.doc.toString());
      const diags = await client.lint();
      return diags.map((d) => ({
        from: Math.min(d.start, length),
        to: Math.min(Math.max(d.end, d.start), length),
        severity: d.severity === "hint" ? "info" : d.severity,
        message: d.message,
        source: "pgls",
      }));
    } catch {
      return [];
    }
  };
}

function pglsHover(client: PglsClient): Extension {
  return hoverTooltip(async (view, pos) => {
    let text: string | null;
    try {
      await client.update(view.state.doc.toString());
      text = await client.hover(pos);
    } catch {
      return null;
    }
    if (!text) return null;
    return {
      pos,
      create() {
        // Markdown as plain text: no renderer dependency, and no HTML from pgls reaches the DOM.
        const dom = document.createElement("pre");
        dom.className = "cm-pgls-hover";
        dom.textContent = hoverText(text);
        return { dom };
      },
    };
  });
}

export function sqlExtensions(client: PglsClient | null): Extension[] {
  if (!client) return [sql({ dialect: PostgreSQL }), autocompletion()];
  const language = PostgreSQL.language;
  return [
    new LanguageSupport(language, [language.data.of({ autocomplete: pglsCompletionSource(client) })]),
    autocompletion(),
    linter(pglsLintSource(client), { delay: LINT_DELAY_MS }),
    pglsHover(client),
  ];
}
