-- Non-destructive: who owns each order (the end user's wallet / account, lowercased), so a user can
-- list THEIR orders after proving control of the wallet. NULL for custodial (server-account) orders.
ALTER TABLE exec_orders ADD COLUMN IF NOT EXISTS owner TEXT;
UPDATE exec_orders
   SET owner = lower(COALESCE(order_json->'venue_meta'->'user'->>'owner',
                              order_json->'venue_meta'->'user'->>'wallet',
                              order_json->'venue_meta'->'user'->>'maker',
                              order_json->'venue_meta'->'user'->>'address',
                              order_json->'venue_meta'->'user'->>'account'))
 WHERE owner IS NULL;
CREATE INDEX IF NOT EXISTS exec_orders_owner ON exec_orders (owner, created_at DESC) WHERE owner IS NOT NULL;
