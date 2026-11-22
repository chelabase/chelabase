-- The data API's second login, `chelabase_authenticator_elevated`: the one login that holds
-- the roles only an Admin may key or act as, so the database can hold that rule itself.
--
-- `branches.authenticator_elevated_password_enc`: its password, sealed with
-- `Purpose::AuthenticatorElevated` (AAD = branch id). `authenticator_elevated_password_salt`:
-- the SCRAM salt its verifier is computed with (not secret), chosen once with the password;
-- the worker sets the role's password to that verifier. Both or neither, as storage's login.
ALTER TABLE branches
    ADD COLUMN authenticator_elevated_password_enc text,
    ADD COLUMN authenticator_elevated_password_salt bytea
        CHECK (octet_length(authenticator_elevated_password_salt) = 16),
    ADD CONSTRAINT branches_authenticator_elevated_password_with_salt
        CHECK ((authenticator_elevated_password_enc IS NULL)
               = (authenticator_elevated_password_salt IS NULL));
