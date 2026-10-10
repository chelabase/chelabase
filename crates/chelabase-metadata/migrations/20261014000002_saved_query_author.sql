-- Members and roles: who wrote a saved query. The author (or an Admin) may edit or delete it.
-- Rows saved before this have no author (NULL): only an Admin may edit or delete those.
ALTER TABLE saved_queries ADD COLUMN author_member_id uuid REFERENCES members(id);
