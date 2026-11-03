-- Zitadel is gone (#126): nothing binds a tenant or a project's app users to a Zitadel org any
-- more. Members find their tenant through the member token's `tid`, the sign-up gate uses the
-- seeded tenant, and app users live in their project's identity pool. Dropping each column
-- drops its UNIQUE constraint with it.
ALTER TABLE tenants DROP COLUMN zitadel_org_id;
ALTER TABLE project_auth DROP COLUMN zitadel_org_id;
