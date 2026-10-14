-- R17 (user, 2026-10-14): a project Admin's grant is every role on the project (still narrowed
-- per Chelabase database by overrides), so an admin grant names no roles: the API writes none,
-- and the access check reads an admin grant as every role. Existing admin grants drop the
-- roles they named; a grant moved below admin later names its roles again.
UPDATE project_grants SET pg_roles = '{}', updated_at = now()
  WHERE platform_role = 'admin' AND pg_roles <> '{}';
