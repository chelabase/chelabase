-- Step 6g, T14 (E33): a secret key may use a role with BYPASSRLS. Whether the key's role has
-- it, as the worker's check read it at create or rotation and the resync keeps it (by the
-- role's OID). Studio marks such keys "bypasses RLS". Existing keys start false: until E33 a
-- BYPASSRLS role couldn't back a key.
ALTER TABLE data_api_keys
    ADD COLUMN role_bypass_rls boolean NOT NULL DEFAULT false
        CONSTRAINT data_api_keys_bypass_rls_secret_only CHECK (kind = 'secret' OR NOT role_bypass_rls);
