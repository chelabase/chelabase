-- Edge functions m3a: a function's run-as key is a secret key stored sealed (Purpose::DataKey,
-- AAD the key's id), so the functions service can hand it to the function. Every other secret
-- key stays unsealed, and a public key is always sealed. Revoking a managed key clears its
-- sealed value before `managed_by_function_id` can go null (a function delete).
ALTER TABLE data_api_keys DROP CONSTRAINT data_api_keys_check;
ALTER TABLE data_api_keys ADD CONSTRAINT data_api_keys_sealed CHECK (
    CASE WHEN kind = 'public' THEN sealed IS NOT NULL
         ELSE sealed IS NULL OR managed_by_function_id IS NOT NULL
    END
);
