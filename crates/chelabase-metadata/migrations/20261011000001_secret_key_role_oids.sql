-- A secret key is bound to its role's identity on the compute (`pg_roles.oid`), not to its
-- name. Stored at creation and rotation from the worker's role check; the data API runs a key
-- only while the role named `role` still has this OID, and the resync revokes keys whose OID
-- is gone, follows a rename the DDL receiver didn't see, and fills the OID of keys made before
-- this column existed. NULL: a public key (role `anon`), or a secret key not backfilled yet.
ALTER TABLE data_api_keys
    ADD COLUMN role_oid bigint CHECK (role_oid IS NULL OR (kind = 'secret' AND role_oid > 0));
