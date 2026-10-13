-- Non-destructive: integrator accounts (API keys stored as SHA-256 only).
CREATE TABLE IF NOT EXISTS integrators (
    id          UUID PRIMARY KEY,
    key_hash    TEXT        NOT NULL UNIQUE,
    record      JSONB       NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
