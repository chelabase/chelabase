-- A secret key is bound to its role's OID *on a timeline* (E26): a reset, restore or a merge's
-- reset brings a catalog whose OID counter diverged at the branch point, so the same OID can
-- name a different role there. `role_timeline` is the Neon timeline `role_oid` was read on.
-- The resync follows a rename only on that timeline; on another one it keeps the key only while
-- the OID still carries the key's role name (and stamps the new timeline), else it revokes it.
-- NULL: no OID yet, or a key made before this column existed. Existing rows stay NULL rather
-- than taking the branch's current timeline: their OID may have been read before a switch, and
-- the resync treats a missing timeline like another timeline's.
ALTER TABLE data_api_keys
    ADD COLUMN role_timeline text
        CHECK (role_timeline IS NULL OR (role_oid IS NOT NULL AND role_timeline <> ''));
