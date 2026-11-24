-- Accounts, sessions, mail tokens, world tickets, rate limits and the audit log.
-- Every secret (session token, mail token, world ticket) is stored as its SHA-256 only.

CREATE TABLE accounts (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email             TEXT NOT NULL,
    -- lower-cased and trimmed: one account per address however it is typed
    email_norm        TEXT NOT NULL UNIQUE,
    username          TEXT NOT NULL,
    -- lower-cased: "Ash" and "ash" are one name
    username_norm     TEXT NOT NULL UNIQUE,
    -- what other players see (redex_proto::PlayerId::Account): random, never the sequential id
    public_id         BIGINT NOT NULL UNIQUE CHECK (public_id > 0),
    password_hash     TEXT NOT NULL,
    email_verified_at TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at       TIMESTAMPTZ
);

CREATE TABLE account_settings (
    account_id     BIGINT PRIMARY KEY REFERENCES accounts (id) ON DELETE CASCADE,
    language       TEXT NOT NULL DEFAULT 'en',
    show_online    BOOLEAN NOT NULL DEFAULT TRUE,
    allow_whispers BOOLEAN NOT NULL DEFAULT TRUE,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE trainer_profiles (
    account_id   BIGINT PRIMARY KEY REFERENCES accounts (id) ON DELETE CASCADE,
    trainer_name TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- the account's characters (redex_proto::Character): at most 8, newest `changed` wins (redex_proto::merge)
CREATE TABLE account_characters (
    account_id   BIGINT NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    character_id BIGINT NOT NULL CHECK (character_id <> 0),
    name         TEXT NOT NULL,
    look         BYTEA NOT NULL,
    created_ms   BIGINT NOT NULL,
    changed_ms   BIGINT NOT NULL,
    PRIMARY KEY (account_id, character_id)
);

CREATE TABLE sessions (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id   BIGINT NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    token_hash   BYTEA NOT NULL UNIQUE,
    device       TEXT NOT NULL,
    ip           TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL,
    revoked_at   TIMESTAMPTZ
);
CREATE INDEX sessions_account ON sessions (account_id);

CREATE TABLE email_tokens (
    token_hash BYTEA PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    purpose    TEXT NOT NULL CHECK (purpose IN ('verify', 'reset')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    used_at    TIMESTAMPTZ
);
CREATE INDEX email_tokens_account ON email_tokens (account_id, purpose);

CREATE TABLE world_tickets (
    ticket_hash BYTEA PRIMARY KEY,
    account_id  BIGINT NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    session_id  BIGINT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    world       TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    used_at     TIMESTAMPTZ
);
CREATE INDEX world_tickets_expiry ON world_tickets (expires_at);

-- fixed-window counters: one row per (key, window start)
CREATE TABLE rate_limits (
    key          TEXT NOT NULL,
    window_start TIMESTAMPTZ NOT NULL,
    count        INTEGER NOT NULL,
    PRIMARY KEY (key, window_start)
);
CREATE INDEX rate_limits_window ON rate_limits (window_start);

CREATE TABLE audit_log (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT REFERENCES accounts (id) ON DELETE SET NULL,
    event      TEXT NOT NULL,
    ip         TEXT,
    detail     JSONB NOT NULL DEFAULT '{}'::jsonb,
    at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX audit_log_account ON audit_log (account_id, at);
