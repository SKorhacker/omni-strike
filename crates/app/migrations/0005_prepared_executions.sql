-- Non-destructive: non-custodial executions waiting for the user's signatures (survive a restart and
-- are shared by every API instance; `taken_at` makes the submit single-use across instances), and an
-- index for the rolling spend window of the risk caps.
CREATE TABLE IF NOT EXISTS prepared_executions (
    route_id    UUID PRIMARY KEY,
    mode        TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL,
    expires_at  TIMESTAMPTZ NOT NULL,
    taken_at    TIMESTAMPTZ,
    payload     JSONB       NOT NULL
);
CREATE INDEX IF NOT EXISTS prepared_executions_open ON prepared_executions (expires_at) WHERE taken_at IS NULL;
CREATE INDEX IF NOT EXISTS exec_orders_mode_created ON exec_orders (mode, created_at);
