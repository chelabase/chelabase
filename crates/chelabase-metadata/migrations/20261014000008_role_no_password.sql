-- A role with no password at all (a group role made in SQL, or one whose password was
-- removed) is kept as a row, so members can be granted it. It has no verifier, like an MD5
-- row; this column tells the two apart. The proxy treats a NULL verifier as unknown either way.
ALTER TABLE neon_roles ADD COLUMN no_password boolean NOT NULL DEFAULT false;
