-- Pooled endpoints m13 (#238): a function's own SQL login on a branch.
--
-- `branch_functions.sql_access`: whether the function gets a SQL login (on by default once a
-- run-as role is approved). `sql_password_enc`: the login's password, sealed with
-- `Purpose::FunctionsSql` (AAD = branch id + function id); `sql_password_salt`: the SCRAM salt
-- its verifier is computed with (not secret), chosen once with the password. Both or neither,
-- and none while `sql_access` is off (turning it on again makes a new password). A new branch
-- copies `sql_access` with the run-as binding, never the password.
ALTER TABLE branch_functions
    ADD COLUMN sql_access boolean NOT NULL DEFAULT true,
    ADD COLUMN sql_password_enc text,
    ADD COLUMN sql_password_salt bytea CHECK (octet_length(sql_password_salt) = 16),
    ADD CONSTRAINT branch_functions_sql_password_with_salt
        CHECK ((sql_password_enc IS NULL) = (sql_password_salt IS NULL)),
    ADD CONSTRAINT branch_functions_sql_password_needs_access
        CHECK (sql_access OR sql_password_enc IS NULL);

-- The login's proxy row (origin `function`): it names its branch function, carries the
-- verifier the worker set on the compute and the run-as role OID the login was granted, and
-- goes with its state row. Its name is the login's (`cb_fn_` + 12 hex of the function id). It
-- never holds a sealed password (that is the state row's). The proxy answers it only while the
-- state row still has SQL access, a password and that very run-as OID.
ALTER TABLE neon_roles
    DROP CONSTRAINT neon_roles_origin,
    ADD CONSTRAINT neon_roles_origin
        CHECK (origin IN ('owner', 'sql', 'api', 'personal', 'function')),
    ADD COLUMN function_branch_id uuid,
    ADD COLUMN function_id uuid,
    ADD COLUMN function_role_oid bigint,
    ADD CONSTRAINT neon_roles_function_binding
        CHECK ((origin = 'function') = (function_branch_id IS NOT NULL AND function_id IS NOT NULL
                                        AND function_role_oid IS NOT NULL)),
    ADD CONSTRAINT neon_roles_function_name
        CHECK (origin <> 'function'
               OR name = 'cb_fn_' || left(replace(function_id::text, '-', ''), 12)),
    ADD CONSTRAINT neon_roles_function_unsealed
        CHECK (origin <> 'function' OR password_enc IS NULL),
    ADD CONSTRAINT neon_roles_function_state
        FOREIGN KEY (function_branch_id, function_id)
        REFERENCES branch_functions (branch_id, function_id) ON DELETE CASCADE;
-- The cascade's lookup, and the worker's.
CREATE INDEX neon_roles_function ON neon_roles (function_branch_id, function_id)
    WHERE function_id IS NOT NULL;
