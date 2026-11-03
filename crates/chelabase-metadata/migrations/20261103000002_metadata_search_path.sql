-- Make `public` the default schema for every connection to this database, including the ones
-- that don't pin it (psql, scripts). The metadata pools already set `search_path=public`.
--
-- First, refuse to continue unless the `chelabase` schema from the previous migration is what
-- that migration meant to create: empty, and owned by the migrating role. A schema owned by
-- another role, or one already holding objects, could shadow `public` through `"$user"`.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_namespace WHERE nspname = 'chelabase' AND pg_get_userbyid(nspowner) = current_user
    ) THEN
        RAISE EXCEPTION 'schema chelabase must exist and be owned by %', current_user;
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_class c WHERE c.relnamespace = 'chelabase'::regnamespace
        UNION ALL SELECT 1 FROM pg_proc p WHERE p.pronamespace = 'chelabase'::regnamespace
        UNION ALL SELECT 1 FROM pg_type t WHERE t.typnamespace = 'chelabase'::regnamespace
            AND t.typrelid = 0 AND t.typelem = 0
        UNION ALL SELECT 1 FROM pg_operator o WHERE o.oprnamespace = 'chelabase'::regnamespace
        UNION ALL SELECT 1 FROM pg_collation k WHERE k.collnamespace = 'chelabase'::regnamespace
        UNION ALL SELECT 1 FROM pg_conversion v WHERE v.connamespace = 'chelabase'::regnamespace
    ) THEN
        RAISE EXCEPTION 'schema chelabase must contain no objects';
    END IF;
    EXECUTE format('ALTER DATABASE %I SET search_path = public', current_database());
END
$$;
