-- R40: a person who signed up by themselves (open sign-up) is a `pending` member until an admin
-- approves or declines them.
ALTER TABLE members DROP CONSTRAINT members_status_check;
ALTER TABLE members ADD CONSTRAINT members_status_check
  CHECK (status IN ('invited','pending','active','removed'));
