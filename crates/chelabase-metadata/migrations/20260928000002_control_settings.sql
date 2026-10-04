-- Control-plane settings. `secrets_key_check` holds a value sealed with the
-- secrets key, so services can detect a lost or replaced key at startup.
CREATE TABLE control_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
