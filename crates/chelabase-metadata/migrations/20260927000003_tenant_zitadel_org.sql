-- Soft-bind Zitadel org → Chelabase tenant (nullable until bootstrap).
ALTER TABLE tenants
    ADD COLUMN zitadel_org_id TEXT UNIQUE;
