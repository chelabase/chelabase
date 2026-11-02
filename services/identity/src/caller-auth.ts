// Caller authentication: one static bearer token per internal caller, compared in constant
// time. Nothing else is a credential here; the gateway's internal identity token in
// particular is just an unknown string.
import { createHash, timingSafeEqual } from "node:crypto";
import type { IncomingHttpHeaders } from "node:http";
import type { Caller } from "./config.ts";

const digest = (s: string) => createHash("sha256").update(s, "utf8").digest();

export class CallerAuth {
  private readonly digests: [Caller, Buffer][];

  constructor(tokens: ReadonlyMap<Caller, string>) {
    this.digests = [...tokens].map(([caller, token]) => [caller, digest(token)]);
  }

  /** The caller whose token is presented as `Authorization: Bearer <token>`, or null. */
  identify(headers: IncomingHttpHeaders): Caller | null {
    const header = headers.authorization;
    if (typeof header !== "string" || !header.startsWith("Bearer ")) return null;
    const token = header.slice("Bearer ".length);
    if (token === "") return null;
    // Equal-length digests for timingSafeEqual, and every caller is compared, so timing
    // shows neither how much matched nor which caller it was.
    const presented = digest(token);
    let found: Caller | null = null;
    for (const [caller, expected] of this.digests) {
      if (timingSafeEqual(presented, expected)) found = caller;
    }
    return found;
  }
}
