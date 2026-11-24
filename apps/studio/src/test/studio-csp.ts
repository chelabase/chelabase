// The Content-Security-Policy the Studio image ships (#306), rendered for tests the way the image
// renders it: infra/images/studio-entrypoint.sh derives the API origins from the environment, and
// Caddy substitutes them into infra/images/studio.Caddyfile's `{$NAME}` placeholders at parse time.
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const images = resolve(__dirname, "../../../../infra/images");
export const ENTRYPOINT = resolve(images, "studio-entrypoint.sh");
export const CADDYFILE = resolve(images, "studio.Caddyfile");

export type EntryEnv = { CHELABASE_PUBLIC_SCHEME?: string; CHELABASE_API_HOST?: string };

/** What the entry point exports for `env`, or throws with its stderr. */
export function entrypointOrigins(env: EntryEnv): { api: string; ws: string } {
  const out = execFileSync("sh", [ENTRYPOINT, "sh", "-c", 'printf "%s\\n%s" "$CHELABASE_STUDIO_API_ORIGIN" "$CHELABASE_STUDIO_API_WS_ORIGIN"'], {
    env: { PATH: process.env.PATH, ...env },
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  const [api, ws] = out.split("\n");
  return { api, ws };
}

/** The Caddyfile's Content-Security-Policy value, placeholders unsubstituted. */
export function policyTemplate(): string {
  const m = /Content-Security-Policy "([^"]*)"/.exec(readFileSync(CADDYFILE, "utf8"));
  if (!m) throw new Error(`no Content-Security-Policy header in ${CADDYFILE}`);
  return m[1];
}

/** The policy a container started with `env` sends (the nonce stays Caddy's request placeholder). */
export function renderedPolicy(env: EntryEnv): string {
  const { api, ws } = entrypointOrigins(env);
  const vars: Record<string, string> = { CHELABASE_STUDIO_API_ORIGIN: api, CHELABASE_STUDIO_API_WS_ORIGIN: ws };
  return policyTemplate().replace(/\{\$([A-Z0-9_]+)\}/g, (_m, name: string) => {
    if (!(name in vars)) throw new Error(`the Caddyfile's policy uses {$${name}}, which the entry point doesn't set`);
    return vars[name];
  });
}

/** A policy as directive → source list. */
export function directives(policy: string): Map<string, string[]> {
  const map = new Map<string, string[]>();
  for (const part of policy.split(";")) {
    const [name, ...sources] = part.trim().split(/\s+/);
    if (name) map.set(name.toLowerCase(), sources);
  }
  return map;
}
