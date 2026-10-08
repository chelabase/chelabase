-- Rehearsal and static endpoints are unroutable: the proxy never connects to them, even though
-- rehearsal branches copy role verifiers. `static_lsn` pins a read-only static compute.
ALTER TABLE neon_endpoints ADD COLUMN routable boolean NOT NULL DEFAULT true;
ALTER TABLE neon_endpoints ADD COLUMN static_lsn text NULL;
