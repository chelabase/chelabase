-- Members hub step 1, D10: when a member was suspended and by whom. Suspend writes both in its
-- transaction (a retry on an already-suspended member keeps the first values); reinstate
-- clears both. A removed member that was suspended keeps them, so the CHECKs do not tie the
-- columns to `status = 'suspended'` alone: `suspended_by` needs `suspended_at`, and a
-- suspended member has `suspended_at`. `suspended_by` may be null (a backfilled row).
ALTER TABLE members ADD COLUMN suspended_at timestamptz;
ALTER TABLE members ADD COLUMN suspended_by uuid REFERENCES members(id);

-- Existing suspended rows (before the CHECKs): the latest `member_suspended` event's time and
-- actor, else `updated_at` with no actor.
UPDATE members m
   SET suspended_at = coalesce(
         (SELECT e.at FROM access_events e
           WHERE e.target_member_id = m.id AND e.action = 'member_suspended'
           ORDER BY e.at DESC, e.id DESC LIMIT 1),
         m.updated_at),
       suspended_by = (SELECT e.actor_member_id FROM access_events e
           WHERE e.target_member_id = m.id AND e.action = 'member_suspended'
           ORDER BY e.at DESC, e.id DESC LIMIT 1)
 WHERE m.status = 'suspended';

ALTER TABLE members ADD CONSTRAINT members_suspended_by_needs_at
  CHECK (suspended_by IS NULL OR suspended_at IS NOT NULL);
ALTER TABLE members ADD CONSTRAINT members_suspended_has_at
  CHECK (status <> 'suspended' OR suspended_at IS NOT NULL);
