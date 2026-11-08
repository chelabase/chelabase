-- Edge functions m13b (#186): the functions service delivers database events as its own login.
--
-- `branches.functions_password_enc`: the `chelabase_functions_dispatch` login's password,
-- sealed with `Purpose::FunctionsDispatch` (AAD = branch id). `functions_password_salt`: the
-- SCRAM salt its verifier is computed with (not secret), chosen once with the password; the
-- worker sets the role's password to that verifier. Both or neither, as storage's login.
ALTER TABLE branches
    ADD COLUMN functions_password_enc text,
    ADD COLUMN functions_password_salt bytea CHECK (octet_length(functions_password_salt) = 16),
    ADD CONSTRAINT branches_functions_password_with_salt
        CHECK ((functions_password_enc IS NULL) = (functions_password_salt IS NULL));

-- Events the outbox bound removed without delivering them (dead events past the cap or the
-- retention, and the queued events of a trigger that was disabled): never a pending event of a
-- delivered trigger.
ALTER TABLE function_triggers
    ADD COLUMN dropped_count bigint NOT NULL DEFAULT 0 CHECK (dropped_count >= 0);
