-- Object storage m18: webhook endpoints for storage object events, and their recent attempts.
--
-- An endpoint belongs to one Chelabase database and gets that database's object events (every
-- branch), filtered by event type, and optionally by bucket and name prefix. `secret_enc` is its
-- signing secret, sealed (chelabase-secrets `Purpose::Webhook`, bound to the endpoint id): shown
-- once at creation and at rotation, never read back by the API.
--
-- `enabled_since` is when delivery (re)started: the worker's consumer for the endpoint delivers
-- events from then on. `failing_since` is the start of the current run of failed attempts (null
-- after a success); three days of it disable the endpoint (`disabled_reason = 'failing'`).
CREATE TABLE storage_webhooks (
    id                   uuid PRIMARY KEY,
    database_id          uuid NOT NULL REFERENCES databases (id) ON DELETE CASCADE,
    url                  text NOT NULL CHECK (length(url) BETWEEN 1 AND 2048),
    events               text[] NOT NULL CHECK (
        cardinality(events) BETWEEN 1 AND 4
        AND events <@ ARRAY['object.created', 'object.updated', 'object.deleted', 'object.moved']
    ),
    bucket               text CHECK (length(bucket) BETWEEN 1 AND 100),
    prefix               text CHECK (length(prefix) BETWEEN 1 AND 1024),
    secret_enc           text NOT NULL,
    enabled              boolean NOT NULL DEFAULT true,
    enabled_since        timestamptz NOT NULL DEFAULT now(),
    disabled_reason      text CHECK (disabled_reason IN ('member', 'failing')),
    failing_since        timestamptz,
    consecutive_failures integer NOT NULL DEFAULT 0,
    last_attempt_at      timestamptz,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    CHECK (enabled = (disabled_reason IS NULL))
);
CREATE INDEX storage_webhooks_database ON storage_webhooks (database_id);

-- The last attempts per endpoint (status and duration only: never a response body or header),
-- trimmed to the newest 100 on every insert.
CREATE TABLE storage_webhook_attempts (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    webhook_id   uuid NOT NULL REFERENCES storage_webhooks (id) ON DELETE CASCADE,
    event_id     uuid NOT NULL,
    event_type   text NOT NULL,
    attempt      integer NOT NULL,
    outcome      text NOT NULL CHECK (outcome IN ('delivered', 'http_error', 'timeout', 'connect_error', 'refused')),
    status       integer,
    duration_ms  integer NOT NULL,
    attempted_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX storage_webhook_attempts_webhook ON storage_webhook_attempts (webhook_id, id DESC);
