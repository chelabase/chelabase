/**
 * Storage webhooks: verify a delivery's signature and read its event.
 *
 * Chelabase POSTs each storage object event to a registered endpoint as JSON, with
 * `Chelabase-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<raw body>">`, keyed
 * with the endpoint's secret (`whsec_…`). Verify against the raw body exactly as received
 * (before any JSON parsing), and treat an event id seen before as a duplicate: delivery is at
 * least once.
 *
 * @module
 */

/** What happened to an object (`webhook.test`: a test event sent on request). */
export type StorageEventType =
  | "object.created"
  | "object.updated"
  | "object.deleted"
  | "object.moved"
  | "webhook.test";

/** One storage object event, as delivered. */
export interface StorageEvent {
  /** Unique per event; every delivery attempt of it carries the same id. */
  id: string;
  type: StorageEventType;
  /** When the change happened (RFC 3339). */
  time: string;
  database_id: string;
  branch_id: string;
  /** The Postgres database on the branch. */
  pg_database: string;
  bucket: string;
  name: string;
  /** The object's version (null for a delete that had none). */
  version_id: string | null;
  size: number | null;
  content_type: string | null;
  /** The database role the change ran as, and the caller's `sub` claim when it had one. */
  actor: { role: string; sub: string | null };
  /** Only on `object.moved`: where the object was. */
  from?: { bucket: string; name: string };
}

export type WebhookVerificationReason = "malformed" | "expired" | "mismatch";

/** A delivery that doesn't verify: refuse it (answer 400 or 401). */
export class WebhookVerificationError extends Error {
  readonly reason: WebhookVerificationReason;

  constructor(reason: WebhookVerificationReason, message: string) {
    super(message);
    this.name = "WebhookVerificationError";
    this.reason = reason;
  }
}

export interface VerifyWebhookOptions {
  /** How far the signature's timestamp may be from now, in seconds (default 300). */
  toleranceSec?: number;
  /** The current time in milliseconds (tests). */
  now?: () => number;
}

type HeaderSource = Headers | Record<string, string | string[] | undefined>;

const SIGNATURE_HEADER = "chelabase-signature";

function header(headers: HeaderSource, name: string): string | undefined {
  if (typeof (headers as Headers).get === "function") {
    return (headers as Headers).get(name) ?? undefined;
  }
  for (const [key, value] of Object.entries(headers)) {
    if (key.toLowerCase() === name) {
      return Array.isArray(value) ? value[0] : value;
    }
  }
  return undefined;
}

function hexToBytes(hex: string): Uint8Array<ArrayBuffer> | undefined {
  if (hex.length === 0 || hex.length % 2 !== 0 || !/^[0-9a-fA-F]+$/.test(hex)) {
    return undefined;
  }
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

/**
 * Verifies a storage webhook delivery and returns its event. Throws a
 * {@link WebhookVerificationError} when the `Chelabase-Signature` header is missing or
 * malformed, its timestamp is more than `toleranceSec` from now (a replay), or no `v1`
 * matches the body (tampered, or another secret).
 *
 * @param secret The endpoint's secret, as returned at creation or rotation.
 * @param headers The request's headers (a `Headers` or a plain record).
 * @param rawBody The request body exactly as received.
 */
export async function verifyWebhook(
  secret: string,
  headers: HeaderSource,
  rawBody: string | Uint8Array,
  options: VerifyWebhookOptions = {},
): Promise<StorageEvent> {
  const tolerance = options.toleranceSec ?? 300;
  const now = Math.floor((options.now ?? Date.now)() / 1000);
  const value = header(headers, SIGNATURE_HEADER) ?? "";
  let t: number | undefined;
  const signatures: Uint8Array<ArrayBuffer>[] = [];
  for (const part of value.split(",")) {
    const at = part.indexOf("=");
    if (at < 0) continue;
    const key = part.slice(0, at).trim();
    const v = part.slice(at + 1).trim();
    if (key === "t" && /^\d+$/.test(v)) {
      t = Number(v);
    } else if (key === "v1") {
      const bytes = hexToBytes(v);
      if (bytes) signatures.push(bytes);
    }
  }
  if (t === undefined || signatures.length === 0) {
    throw new WebhookVerificationError("malformed", "no valid Chelabase-Signature header");
  }
  if (Math.abs(now - t) > tolerance) {
    throw new WebhookVerificationError("expired", "the signature's timestamp is outside the tolerance");
  }
  const encoder = new TextEncoder();
  const body = typeof rawBody === "string" ? encoder.encode(rawBody) : rawBody;
  const prefix = encoder.encode(`${t}.`);
  const signed = new Uint8Array(prefix.length + body.length);
  signed.set(prefix);
  signed.set(body, prefix.length);
  const key = await crypto.subtle.importKey(
    "raw",
    encoder.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["verify"],
  );
  for (const signature of signatures) {
    // `verify` compares in constant time.
    if (await crypto.subtle.verify("HMAC", key, signature, signed)) {
      const text = typeof rawBody === "string" ? rawBody : new TextDecoder().decode(rawBody);
      return JSON.parse(text) as StorageEvent;
    }
  }
  throw new WebhookVerificationError("mismatch", "the signature doesn't match the body");
}
