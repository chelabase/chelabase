import { createHmac } from "node:crypto";
import { describe, expect, it } from "vitest";
import { verifyWebhook, WebhookVerificationError, type StorageEvent } from "./webhooks.ts";
import * as server from "./server.ts";

const SECRET = "whsec_0123456789abcdef";

const event: StorageEvent = {
  id: "8f6f2a8e-6c55-4a37-9c2b-3a1f2b4c5d6e",
  type: "object.created",
  time: "2026-11-07T10:00:00Z",
  database_id: "0b6c2f0e-1111-4c3e-9a3e-000000000001",
  branch_id: "0b6c2f0e-1111-4c3e-9a3e-000000000001",
  pg_database: "app",
  bucket: "avatars",
  name: "u/1.png",
  version_id: "1f6c2f0e-1111-4c3e-9a3e-000000000009",
  size: 3,
  content_type: "image/png",
  actor: { role: "authenticated", sub: "user-1" },
};

/** What the worker sends: `t=<unix>,v1=<hex HMAC-SHA256 of "<t>.<body>">`. */
function sign(secret: string, t: number, body: string): string {
  return `t=${t},v1=${createHmac("sha256", secret).update(`${t}.${body}`).digest("hex")}`;
}

const NOW = 1_762_509_600; // 2025-11-07T10:00:00Z
const clock = () => NOW * 1000;

describe("verifyWebhook", () => {
  const body = JSON.stringify(event);
  const headers = (signature: string) => new Headers({ "chelabase-signature": signature });

  it("verifies a genuine event and returns it typed", async () => {
    const got = await verifyWebhook(SECRET, headers(sign(SECRET, NOW, body)), body, { now: clock });
    expect(got).toEqual(event);
    // A plain header record works too, in any case, and bytes as the body.
    const plain = { "Chelabase-Signature": sign(SECRET, NOW - 299, body) };
    await expect(
      verifyWebhook(SECRET, plain, new TextEncoder().encode(body), { now: clock }),
    ).resolves.toEqual(event);
  });

  it("matches the Rust signer byte for byte", async () => {
    // The worker's test vector (storage_webhooks.rs).
    const rustBody = '{"id":"x","type":"object.created"}';
    const rust = sign(SECRET, 1_700_000_000, rustBody);
    await expect(
      verifyWebhook(SECRET, headers(rust), rustBody, { now: () => 1_700_000_000_000 }),
    ).resolves.toEqual({ id: "x", type: "object.created" });
  });

  it("rejects a tampered body, signature, timestamp or secret", async () => {
    const genuine = sign(SECRET, NOW, body);
    const tampered = body.replace("u/1.png", "u/2.png");
    const flipped = genuine.slice(0, -1) + (genuine.endsWith("0") ? "1" : "0");
    const cases: [string, string, string][] = [
      ["body", genuine, tampered],
      ["signature", flipped, body],
      ["timestamp", genuine.replace(`t=${NOW}`, `t=${NOW - 1}`), body],
    ];
    for (const [what, signature, raw] of cases) {
      await expect(
        verifyWebhook(SECRET, headers(signature), raw, { now: clock }),
        what,
      ).rejects.toMatchObject({ reason: "mismatch" });
    }
    await expect(
      verifyWebhook("whsec_other", headers(genuine), body, { now: clock }),
    ).rejects.toBeInstanceOf(WebhookVerificationError);
  });

  it("rejects a timestamp outside the tolerance (a replay)", async () => {
    const old = sign(SECRET, NOW - 301, body);
    await expect(verifyWebhook(SECRET, headers(old), body, { now: clock })).rejects.toMatchObject({
      reason: "expired",
    });
    const future = sign(SECRET, NOW + 301, body);
    await expect(verifyWebhook(SECRET, headers(future), body, { now: clock })).rejects.toMatchObject({
      reason: "expired",
    });
    // A wider tolerance accepts it.
    await expect(
      verifyWebhook(SECRET, headers(old), body, { now: clock, toleranceSec: 600 }),
    ).resolves.toEqual(event);
  });

  it("rejects a missing or malformed header", async () => {
    for (const signature of ["", "t=1", "v1=00", "t=x,v1=00", "nonsense"]) {
      await expect(
        verifyWebhook(SECRET, headers(signature), body, { now: clock }),
        signature,
      ).rejects.toMatchObject({ reason: "malformed" });
    }
    await expect(verifyWebhook(SECRET, new Headers(), body, { now: clock })).rejects.toMatchObject({
      reason: "malformed",
    });
  });

  it("accepts any matching v1 (a rotation in flight)", async () => {
    const both = `${sign(SECRET, NOW, body)},v1=${"0".repeat(64)}`;
    await expect(verifyWebhook(SECRET, headers(both), body, { now: clock })).resolves.toEqual(event);
  });

  it("is exported from the server entry", () => {
    expect(server.verifyWebhook).toBe(verifyWebhook);
    expect(server.WebhookVerificationError).toBe(WebhookVerificationError);
  });
});
