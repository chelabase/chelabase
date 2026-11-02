/**
 * Stand-in for m5's `identity_admin` functions until it lands: same names, same name check.
 * Here the test role owns the database, so they need no SECURITY DEFINER.
 */
export const ADMIN_FIXTURE = `
CREATE SCHEMA IF NOT EXISTS identity_admin;
CREATE OR REPLACE FUNCTION identity_admin.create_pool_schema(name text) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
  IF name !~ '^identity_(members|app_[0-9a-f]{32})$' THEN
    RAISE EXCEPTION 'invalid pool schema name';
  END IF;
  EXECUTE format('CREATE SCHEMA IF NOT EXISTS %I', name);
END $$;
CREATE OR REPLACE FUNCTION identity_admin.drop_pool_schema(name text) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
  IF name !~ '^identity_(members|app_[0-9a-f]{32})$' THEN
    RAISE EXCEPTION 'invalid pool schema name';
  END IF;
  EXECUTE format('DROP SCHEMA IF EXISTS %I CASCADE', name);
END $$;
`;
