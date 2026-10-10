-- Members and roles, R31: the last role a member acted as through its member role on a branch,
-- recorded when an act-as run starts. A reconcile hands what the member role still owns (a
-- hand-over that didn't happen) to this role, when the member role could still become it,
-- before dropping the rest.
ALTER TABLE member_branch_roles ADD COLUMN last_acted_role text;
