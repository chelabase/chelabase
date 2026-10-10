-- The invite link carries a random token besides Zitadel's code; only its SHA-256 is stored.
-- It lets the accept page describe an invite (who invited you, to what) before any code is
-- spent. Minted per send, so a resend replaces it; invites made before this have none and
-- describe as unknown.
ALTER TABLE invites ADD COLUMN token_hash bytea CHECK (token_hash IS NULL OR octet_length(token_hash) = 32);
