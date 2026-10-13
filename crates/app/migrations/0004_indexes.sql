-- Non-destructive: indexes for the journal's newest-first reads and for time-range scans of the
-- append-only observations table (BRIN: tiny and nearly free to maintain on time-ordered rows).
CREATE INDEX IF NOT EXISTS exec_orders_created ON exec_orders (created_at DESC);
CREATE INDEX IF NOT EXISTS quote_observations_time_brin ON quote_observations USING brin (observed_at);
