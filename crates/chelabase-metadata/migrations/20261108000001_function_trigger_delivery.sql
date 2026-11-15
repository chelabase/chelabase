-- Edge functions m13 (#175): database events delivered to functions.
--
-- Each trigger counts the events that went dead (10 failed deliveries), and remembers the last
-- delivery error and the last delivery.
ALTER TABLE function_triggers
    ADD COLUMN dead_count bigint NOT NULL DEFAULT 0 CHECK (dead_count >= 0),
    ADD COLUMN last_delivery_error text,
    ADD COLUMN last_delivered_at timestamptz;

-- "Drop, then forget": a table trigger that was deleted or disabled (or whose function was
-- deleted) is recorded here until the worker has synced the branch's databases without it, so
-- its database trigger is dropped even when no other trigger keeps the branch in the sweep.
-- No foreign keys: an insert locks no other row, and a row outlives its trigger and function.
CREATE TABLE function_trigger_drops (
    id         bigserial PRIMARY KEY,
    branch_id  uuid NOT NULL,
    trigger_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX function_trigger_drops_branch ON function_trigger_drops (branch_id);
