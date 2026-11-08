-- Edge functions review: a container rollover switches the active deployment only if nothing
-- else set it meanwhile.
--
-- `deployment_generation` counts the writes of `deployment_id` on the branch state (every
-- `set_active_deployment`, even one naming the deployment already active: a rollback to the
-- serving deployment is a decision too). A rollover reads it when it starts and switches with a
-- compare-and-set on it, so a newer rollover, a PATCH or a rollback made while it ran is never
-- overwritten.
ALTER TABLE branch_functions
    ADD COLUMN deployment_generation bigint NOT NULL DEFAULT 0 CHECK (deployment_generation >= 0);
