-- Non-destructive: execution journal (TESTNET only in this version).

CREATE TABLE IF NOT EXISTS route_quotes (
    id           UUID PRIMARY KEY,
    created_at   TIMESTAMPTZ NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL,
    route_hash   TEXT        NOT NULL,
    mode         TEXT        NOT NULL,
    consumed_at  TIMESTAMPTZ,            -- single use: set atomically by the execution
    quote        JSONB       NOT NULL
);

CREATE TABLE IF NOT EXISTS exec_orders (
    id                UUID PRIMARY KEY,
    route_id          UUID        NOT NULL,
    leg_index         INT         NOT NULL,
    mode              TEXT        NOT NULL,
    platform          TEXT        NOT NULL,
    network           TEXT        NOT NULL,
    native_id         TEXT        NOT NULL,
    side              TEXT        NOT NULL,
    qty_native        TEXT        NOT NULL,
    limit_price       TEXT        NOT NULL,
    salt              TEXT        NOT NULL,
    expected_avg      TEXT        NOT NULL,   -- quote used (per underlying)
    expected_total    TEXT        NOT NULL,
    expected_fee      TEXT,
    precomputed_venue_id TEXT,
    venue_order_id    TEXT,
    state             TEXT        NOT NULL,
    is_final          BOOLEAN     NOT NULL DEFAULT false,
    filled_native     TEXT        NOT NULL DEFAULT '0',
    avg_fill_price    TEXT,
    last_error        TEXT,
    created_at        TIMESTAMPTZ NOT NULL,
    submitted_at      TIMESTAMPTZ,
    acked_at          TIMESTAMPTZ,
    updated_at        TIMESTAMPTZ NOT NULL,
    submit_to_ack_ms  BIGINT,
    order_json        JSONB       NOT NULL,
    last_venue_json   JSONB,
    UNIQUE (route_id, leg_index)
);
CREATE INDEX IF NOT EXISTS exec_orders_open ON exec_orders (is_final) WHERE NOT is_final;

CREATE TABLE IF NOT EXISTS order_events (
    id        BIGSERIAL PRIMARY KEY,
    order_id  UUID        NOT NULL,
    at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    state     TEXT        NOT NULL,
    detail    JSONB       NOT NULL
);
CREATE INDEX IF NOT EXISTS order_events_order ON order_events (order_id);
