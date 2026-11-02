-- Object storage m6: one-time upload URLs.
--
-- A row per upload-sign token, written when storage mints the URL and consumed (`used_at`) by
-- its one upload, whichever storage replica serves it and across restarts: keyed by the token's
-- own id (`token_id`, signed into the URL). `key_id` is the data key that minted it: the upload
-- runs as that key's current role, so a revoked key's URLs fail. `claims` are the minting
-- request's JWT claims (an app user's `sub`), which row level security reads.
CREATE TABLE storage_upload_tokens (
    token_id   uuid PRIMARY KEY,
    key_id     uuid NOT NULL REFERENCES data_api_keys (id) ON DELETE CASCADE,
    claims     jsonb NOT NULL,
    expires_at timestamptz NOT NULL,
    used_at    timestamptz
);
CREATE INDEX storage_upload_tokens_expires_at ON storage_upload_tokens (expires_at);
CREATE INDEX storage_upload_tokens_key ON storage_upload_tokens (key_id);
