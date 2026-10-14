-- Protected branches: the login-role guard switch, its exemptions, the acknowledged review.
ALTER TABLE branches
    ADD COLUMN guard_logins    boolean NOT NULL DEFAULT true,
    ADD COLUMN review_ack_hash text,
    ADD COLUMN review_ack_keys text[] NOT NULL DEFAULT '{}';
CREATE TABLE branch_guard_exemptions (
    branch_id  uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    role_name  text NOT NULL CHECK (octet_length(role_name) BETWEEN 1 AND 63),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, role_name)
);
