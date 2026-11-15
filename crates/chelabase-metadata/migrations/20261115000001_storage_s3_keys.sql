-- s3-endpoint m1: S3 access keys. A key is bound to one branch, one Postgres database on it
-- (`database`, by name) and one role, by identity: `role_oid` read on `role_timeline`, as a
-- secret data key's is, so `DROP ROLE r; CREATE ROLE r` never hands the key to the new role.
--
-- SigV4 is an HMAC over the request, so the verifier needs the secret itself: `secret_enc` is
-- sealed (chelabase-secrets `Purpose::S3Key`, AAD the key's id) and opens only for its own row.
-- A live key always has it; revoking clears it in the same statement, so a revoked key never
-- keeps a secret (as `data_api_keys_revoked_unsealed` does for run-as keys).
CREATE TABLE storage_s3_keys (
    id            uuid PRIMARY KEY,
    branch_id     uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    database      text NOT NULL CHECK (octet_length(database) BETWEEN 1 AND 63),
    role          text NOT NULL CHECK (octet_length(role) BETWEEN 1 AND 63),
    name          text NOT NULL CHECK (length(name) BETWEEN 1 AND 63),
    access_key_id text NOT NULL UNIQUE CHECK (access_key_id ~ '^CBAK[A-Z2-7]{16}$'),
    secret_enc    text,
    role_oid      bigint CHECK (role_oid IS NULL OR role_oid > 0),
    role_timeline text CHECK (role_timeline IS NULL OR role_timeline <> ''),
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_used_at  timestamptz,
    revoked_at    timestamptz,
    CONSTRAINT storage_s3_keys_revoked_unsealed
        CHECK ((revoked_at IS NULL) = (secret_enc IS NOT NULL)),
    CONSTRAINT storage_s3_keys_live_role_identity
        CHECK (revoked_at IS NOT NULL OR (role_oid IS NOT NULL AND role_timeline IS NOT NULL))
);
CREATE INDEX storage_s3_keys_live_branch ON storage_s3_keys (branch_id) WHERE revoked_at IS NULL;
