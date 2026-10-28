-- Selfhost-install m3: the fresh-install claim, gated by the one-time setup code.
--
-- `setup_claims`: which Zitadel user a code-gated sign-up created, one per tenant. While the
-- tenant has no active admin, only this user's first request may claim `tenant_admin`
-- (`ensure_member`, under the tenant lock). The row stays as the audit record of the claim.
-- A stale claim (its user has no member row and is gone from Zitadel) may be rebound by a new
-- code-gated sign-up; `replaced_user_id` keeps the user it replaced.
CREATE TABLE setup_claims (
  tenant_id uuid PRIMARY KEY REFERENCES tenants(id),
  zitadel_user_id text NOT NULL CHECK (zitadel_user_id <> ''),
  created_at timestamptz NOT NULL DEFAULT now(),
  replaced_user_id text CHECK (replaced_user_id <> ''));

-- `setup_claim_nonces`: what a sign-up gate check with the right code hands out, stored as the
-- SHA-256 of the nonce, single use (spending deletes the row) and short-lived. Only a live
-- nonce of the same tenant binds a claim.
CREATE TABLE setup_claim_nonces (
  nonce_hash bytea PRIMARY KEY CHECK (octet_length(nonce_hash) = 32),
  tenant_id uuid NOT NULL REFERENCES tenants(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  expires_at timestamptz NOT NULL,
  CHECK (expires_at > created_at));
CREATE INDEX setup_claim_nonces_expires ON setup_claim_nonces (expires_at);
