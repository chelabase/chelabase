-- Object storage m5: the storage service's own login on each branch, and its connection budget.
--
-- `branches.storage_password_enc`: the `chelabase_storage` login's password, sealed with
-- `Purpose::Storage` (AAD = branch id). `storage_password_salt`: the SCRAM salt its verifier
-- is computed with (not secret), chosen once with the password; the worker sets the role's
-- password to that verifier and its health check requires exactly it. Both or neither.
--
-- `databases.storage_max_connections`: how many connections the storage service may hold to
-- each branch, apart from the data API's `max_connections`.
ALTER TABLE branches
    ADD COLUMN storage_password_enc text,
    ADD COLUMN storage_password_salt bytea CHECK (octet_length(storage_password_salt) = 16),
    ADD CONSTRAINT branches_storage_password_with_salt
        CHECK ((storage_password_enc IS NULL) = (storage_password_salt IS NULL));

ALTER TABLE databases
    ADD COLUMN storage_max_connections integer NOT NULL DEFAULT 2
        CHECK (storage_max_connections BETWEEN 1 AND 20);
