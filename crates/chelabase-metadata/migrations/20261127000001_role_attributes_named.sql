-- #425: an API change that hasn't been applied yet (`attributes_pending`) drives onto the
-- compute only the fields it named; every other attribute follows the catalog, so a change
-- made in SQL since isn't overwritten. `attributes_named` lists those fields, from the
-- vocabulary below. A PATCH names the fields it set (joined to the earlier set while an
-- earlier change is still pending, so neither is lost); a create names every field.
--
-- The default is every field: rows already pending at the upgrade, and any writer that
-- doesn't set the column, keep driving everything, as before. Adding a column with a
-- constant default doesn't rewrite the table.
ALTER TABLE neon_roles
    ADD COLUMN attributes_named text[] NOT NULL
        DEFAULT ARRAY['owner_level', 'login', 'inherit', 'connection_limit', 'valid_until',
                      'bypass_rls', 'member_of']::text[]
        CONSTRAINT neon_roles_attributes_named_known CHECK (
            attributes_named <@ ARRAY['owner_level', 'login', 'inherit', 'connection_limit',
                                      'valid_until', 'bypass_rls', 'member_of']::text[]
        );
