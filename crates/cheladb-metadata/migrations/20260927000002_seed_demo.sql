-- Seed local demo tenant + project (fixed UUIDs for curl / just create-db-demo).
INSERT INTO tenants (id, slug)
VALUES ('00000000-0000-4000-8000-000000000001', 'demo')
ON CONFLICT (id) DO NOTHING;

INSERT INTO projects (id, tenant_id, slug)
VALUES (
    '00000000-0000-4000-8000-000000000002',
    '00000000-0000-4000-8000-000000000001',
    'default'
)
ON CONFLICT (id) DO NOTHING;
