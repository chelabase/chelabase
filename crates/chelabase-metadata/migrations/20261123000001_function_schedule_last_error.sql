-- Why a function schedule was switched off.
--
-- `last_error` is NULL normally. The worker's claim sets it when it disables a schedule whose
-- cron or time zone no longer parses, or that has no time left, and the startup report sets it
-- on a schedule that no longer parses; a claim that advances the schedule and a successful
-- update clear it. The text is only the schedule's own cron or zone and the parser's message,
-- at most 500 characters.
--
-- Additive: no default and no backfill, so the table isn't rewritten (Postgres scans it once
-- to validate the CHECK) and every existing row reads NULL.
ALTER TABLE function_schedules
    ADD COLUMN last_error text CHECK (last_error IS NULL OR length(last_error) BETWEEN 1 AND 500);
