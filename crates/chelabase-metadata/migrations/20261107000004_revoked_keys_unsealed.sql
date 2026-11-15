-- Edge functions m3a fix (audit S1): a revoked secret key never keeps a sealed value, so a
-- function's run-as secret doesn't outlive its key. Every revoke clears it; rows revoked
-- before that fix are cleared here. A public key's sealed value is left as it is.
UPDATE data_api_keys SET sealed = NULL
WHERE kind = 'secret' AND revoked_at IS NOT NULL AND sealed IS NOT NULL;
ALTER TABLE data_api_keys ADD CONSTRAINT data_api_keys_revoked_unsealed
    CHECK (kind = 'public' OR revoked_at IS NULL OR sealed IS NULL);
