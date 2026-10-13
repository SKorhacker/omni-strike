-- Non-destructive initial schema. Never DROP / TRUNCATE in migrations of this project.

CREATE TABLE IF NOT EXISTS quote_observations (
    id               BIGSERIAL PRIMARY KEY,
    request_id       UUID        NOT NULL,
    observed_at      TIMESTAMPTZ NOT NULL,
    platform         TEXT        NOT NULL,
    network          TEXT        NOT NULL,
    native_id        TEXT        NOT NULL,
    side             TEXT        NOT NULL,
    requested_qty    TEXT        NOT NULL,  -- decimal as text: exact, no float
    covered_qty      TEXT        NOT NULL,
    total_estimated  TEXT,                  -- NULL = unknown costs (never zero)
    eligible         BOOLEAN     NOT NULL,
    ineligible       JSONB       NOT NULL,
    age_ms           BIGINT      NOT NULL,
    provenance       TEXT        NOT NULL,
    estimate         JSONB       NOT NULL
);
CREATE INDEX IF NOT EXISTS quote_observations_req ON quote_observations (request_id);
CREATE INDEX IF NOT EXISTS quote_observations_time ON quote_observations (observed_at);

CREATE TABLE IF NOT EXISTS simulations (
    id          UUID PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL,
    status      TEXT        NOT NULL,
    platform    TEXT,
    request     JSONB       NOT NULL,
    record      JSONB       NOT NULL
);
CREATE INDEX IF NOT EXISTS simulations_time ON simulations (created_at);

CREATE TABLE IF NOT EXISTS audit_log (
    id          BIGSERIAL PRIMARY KEY,
    at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    action      TEXT        NOT NULL,
    subject     TEXT,
    detail      JSONB       NOT NULL
);
