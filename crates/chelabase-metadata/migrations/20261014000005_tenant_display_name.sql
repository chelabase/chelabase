-- R36: a tenant's display name, shown instead of its slug. NULL means "use the slug"; a stored
-- name is trimmed, not empty, at most 80 characters and has no control characters.
ALTER TABLE tenants ADD COLUMN display_name text
  CHECK (display_name IS NULL OR (display_name = btrim(display_name)
    AND display_name <> '' AND char_length(display_name) <= 80
    AND display_name !~ '[[:cntrl:]]'));
