-- Soft-bind Zitadel org → Cheladb tenant (nullable until bootstrap).
ALTER TABLE tenants
    ADD COLUMN zitadel_org_id TEXT UNIQUE;
