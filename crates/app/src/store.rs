//! Persistence. PostgreSQL in production (`PgStore`); `MemStore` exists ONLY for tests and
//! is never selected by the server binary.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oa_core::cost::QuoteEstimate;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Mutex;
use std::time::Duration;
use uuid::Uuid;

use crate::execution::{ConsumeOutcome, OrderRow, StoredRoute};

#[derive(Debug, thiserror::Error)]
#[error("persistence error: {0}")]
pub struct StoreError(pub String);

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        // sqlx errors do not include the connection password; keep the message short anyway.
        StoreError(e.to_string())
    }
}

#[async_trait]
pub trait Store: Send + Sync {
    async fn ping(&self) -> Result<(), StoreError>;
    async fn record_quotes(
        &self,
        request_id: Uuid,
        at: DateTime<Utc>,
        quotes: &[QuoteEstimate],
    ) -> Result<(), StoreError>;
    /// Several comparisons' observations at once (the batching writer). Default: one call each.
    async fn record_quote_batches(&self, batches: &[ObservationBatch]) -> Result<(), StoreError> {
        for b in batches {
            self.record_quotes(b.request_id, b.at, &b.quotes).await?;
        }
        Ok(())
    }
    async fn save_simulation(
        &self,
        id: Uuid,
        at: DateTime<Utc>,
        status: &str,
        platform: Option<&str>,
        request: &Value,
        record: &Value,
    ) -> Result<(), StoreError>;
    async fn get_simulation(&self, id: Uuid) -> Result<Option<Value>, StoreError>;
    async fn audit(
        &self,
        action: &str,
        subject: Option<&str>,
        detail: &Value,
    ) -> Result<(), StoreError>;
    fn kind(&self) -> &'static str;

    // --- execution journal (TESTNET)
    async fn save_route(&self, r: &StoredRoute) -> Result<(), StoreError>;
    /// Atomic single-use consumption (hash-bound, unexpired).
    async fn consume_route(
        &self,
        id: Uuid,
        hash: &str,
        now: DateTime<Utc>,
    ) -> Result<ConsumeOutcome, StoreError>;
    /// Insert or update the order AND append an event, in one transaction.
    async fn upsert_order(
        &self,
        o: &OrderRow,
        event: &str,
        detail: &Value,
    ) -> Result<(), StoreError>;
    async fn get_order(&self, id: Uuid) -> Result<Option<OrderRow>, StoreError>;
    async fn list_orders(&self, limit: i64) -> Result<Vec<OrderRow>, StoreError>;
    /// The orders of one end user (`owner`, lowercased) that were actually sent, most recent first.
    /// Preparations never signed are left out: anyone can prepare for any address.
    async fn list_orders_by_owner(
        &self,
        owner: &str,
        limit: i64,
    ) -> Result<Vec<OrderRow>, StoreError>;
    async fn open_orders(&self) -> Result<Vec<OrderRow>, StoreError>;
    /// Integrator accounts: (key hash, JSON record). Write-through from `integrators::Integrators`.
    async fn upsert_integrator(
        &self,
        id: Uuid,
        key_hash: &str,
        record: &Value,
    ) -> Result<(), StoreError>;
    async fn load_integrators(&self) -> Result<Vec<(String, Value)>, StoreError>;

    // --- non-custodial executions waiting for the user's signatures
    async fn save_prepared(
        &self,
        route_id: Uuid,
        mode: &str,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        payload: &Value,
    ) -> Result<(), StoreError>;
    /// The payload of an unexpired prepared execution nobody has taken yet (does not consume it).
    async fn get_prepared(
        &self,
        route_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<Value>, StoreError>;
    /// Atomic single-use take (across every instance sharing the database). False = already taken
    /// or expired in the meantime.
    async fn take_prepared(&self, route_id: Uuid, now: DateTime<Utc>) -> Result<bool, StoreError>;
    /// Committed buy cost (limit x qty + fee ceiling) of this mode's orders created since `since`:
    /// orders open, being sent, or closed with a fill (counted at their full limit: conservative).
    /// Unsigned VALIDATED rows and orders closed with NOTHING filled (expired unsigned, refused
    /// before sending, rejected or cancelled by the venue) do not count.
    async fn committed_spend_since(
        &self,
        mode: &str,
        since: DateTime<Utc>,
    ) -> Result<rust_decimal::Decimal, StoreError>;
    /// Retention: route quotes and prepared executions expired before `expired_before`, simulations
    /// created before `simulations_before`. The order journal and the audit log are never purged.
    /// Returns the number of rows deleted.
    async fn purge(
        &self,
        expired_before: DateTime<Utc>,
        simulations_before: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        let _ = (expired_before, simulations_before);
        Ok(0)
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, StoreError> {
    serde_json::to_value(v).map_err(|e| StoreError(e.to_string()))
}

fn from_json<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, StoreError> {
    serde_json::from_value(v).map_err(|e| StoreError(e.to_string()))
}

/// The observations of one comparison, queued for the batching writer.
#[derive(Debug, Clone)]
pub struct ObservationBatch {
    pub request_id: Uuid,
    pub at: DateTime<Utc>,
    pub quotes: Vec<QuoteEstimate>,
}

pub struct PgStore {
    pool: PgPool,
}

/// Rows per INSERT statement (14 bind parameters per row, well under Postgres' 65,535 limit).
const OBS_ROWS_PER_STATEMENT: usize = 1_000;

impl PgStore {
    /// Connects and applies the embedded, non-destructive migrations.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let max = std::env::var("OA_DB_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(16u32)
            .clamp(2, 200);
        let pool = PgPoolOptions::new()
            .max_connections(max)
            .acquire_timeout(Duration::from_secs(5))
            // Recycle connections (server restarts, failovers, leaks) and never let one statement
            // hold a connection forever.
            .idle_timeout(Duration::from_secs(600))
            .max_lifetime(Duration::from_secs(1800))
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout = '15s'")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| StoreError(format!("migration: {e}")))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl Store for PgStore {
    async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    async fn record_quote_batches(&self, batches: &[ObservationBatch]) -> Result<(), StoreError> {
        let rows: Vec<(Uuid, DateTime<Utc>, &QuoteEstimate)> = batches
            .iter()
            .flat_map(|b| b.quotes.iter().map(move |q| (b.request_id, b.at, q)))
            .collect();
        for chunk in rows.chunks(OBS_ROWS_PER_STATEMENT) {
            let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                "INSERT INTO quote_observations (request_id, observed_at, platform, network, \
                 native_id, side, requested_qty, covered_qty, total_estimated, eligible, \
                 ineligible, age_ms, provenance, estimate) ",
            );
            qb.push_values(chunk, |mut b, (rid, at, q)| {
                b.push_bind(*rid)
                    .push_bind(*at)
                    .push_bind(q.instrument.platform.as_str())
                    .push_bind(q.instrument.network.clone())
                    .push_bind(q.instrument.native_id.clone())
                    .push_bind(format!("{:?}", q.side))
                    .push_bind(q.requested_underlying.to_string())
                    .push_bind(q.covered_underlying.to_string())
                    .push_bind(q.total_estimated.map(|d| d.to_string()))
                    .push_bind(q.eligible())
                    .push_bind(serde_json::to_value(&q.ineligible).unwrap_or(Value::Null))
                    .push_bind(q.age_ms)
                    .push_bind(format!("{:?}", q.provenance))
                    .push_bind(serde_json::to_value(q).unwrap_or(Value::Null));
            });
            qb.build().execute(&self.pool).await?;
        }
        Ok(())
    }

    async fn record_quotes(
        &self,
        request_id: Uuid,
        at: DateTime<Utc>,
        quotes: &[QuoteEstimate],
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        for q in quotes {
            sqlx::query(
                "INSERT INTO quote_observations (request_id, observed_at, platform, network, \
                 native_id, side, requested_qty, covered_qty, total_estimated, eligible, \
                 ineligible, age_ms, provenance, estimate) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
            )
            .bind(request_id)
            .bind(at)
            .bind(q.instrument.platform.as_str())
            .bind(&q.instrument.network)
            .bind(&q.instrument.native_id)
            .bind(format!("{:?}", q.side))
            .bind(q.requested_underlying.to_string())
            .bind(q.covered_underlying.to_string())
            .bind(q.total_estimated.map(|d| d.to_string()))
            .bind(q.eligible())
            .bind(serde_json::to_value(&q.ineligible).unwrap_or(Value::Null))
            .bind(q.age_ms)
            .bind(format!("{:?}", q.provenance))
            .bind(serde_json::to_value(q).unwrap_or(Value::Null))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn save_simulation(
        &self,
        id: Uuid,
        at: DateTime<Utc>,
        status: &str,
        platform: Option<&str>,
        request: &Value,
        record: &Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO simulations (id, created_at, status, platform, request, record) \
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(id)
        .bind(at)
        .bind(status)
        .bind(platform)
        .bind(request)
        .bind(record)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_simulation(&self, id: Uuid) -> Result<Option<Value>, StoreError> {
        let row = sqlx::query("SELECT record FROM simulations WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<Value, _>("record")))
    }

    async fn audit(
        &self,
        action: &str,
        subject: Option<&str>,
        detail: &Value,
    ) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO audit_log (action, subject, detail) VALUES ($1,$2,$3)")
            .bind(action)
            .bind(subject)
            .bind(detail)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "postgres"
    }

    async fn save_route(&self, r: &StoredRoute) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO route_quotes (id, created_at, expires_at, route_hash, mode, quote) VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(r.id)
        .bind(r.created_at)
        .bind(r.expires_at)
        .bind(&r.route_hash)
        .bind(format!("{:?}", r.mode))
        .bind(to_json(r)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn consume_route(
        &self,
        id: Uuid,
        hash: &str,
        now: DateTime<Utc>,
    ) -> Result<ConsumeOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT route_hash, expires_at, consumed_at, quote FROM route_quotes WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Ok(ConsumeOutcome::NotFound);
        };
        let (h, exp, consumed): (String, DateTime<Utc>, Option<DateTime<Utc>>) = (
            row.get("route_hash"),
            row.get("expires_at"),
            row.get("consumed_at"),
        );
        let out = if consumed.is_some() {
            ConsumeOutcome::AlreadyConsumed
        } else if h != hash {
            ConsumeOutcome::HashMismatch
        } else if now >= exp {
            ConsumeOutcome::Expired
        } else {
            sqlx::query(
                "UPDATE route_quotes SET consumed_at = $2 WHERE id = $1 AND consumed_at IS NULL",
            )
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            ConsumeOutcome::Consumed(Box::new(from_json(row.get::<Value, _>("quote"))?))
        };
        tx.commit().await?;
        Ok(out)
    }

    async fn upsert_order(
        &self,
        o: &OrderRow,
        event: &str,
        detail: &Value,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO exec_orders (id, route_id, leg_index, mode, platform, network, native_id, side, qty_native, \
             limit_price, salt, expected_avg, expected_total, expected_fee, precomputed_venue_id, venue_order_id, state, \
             is_final, filled_native, avg_fill_price, last_error, created_at, submitted_at, acked_at, updated_at, \
             submit_to_ack_ms, order_json, last_venue_json, owner) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29) \
             ON CONFLICT (id) DO UPDATE SET venue_order_id = EXCLUDED.venue_order_id, state = EXCLUDED.state, \
             is_final = EXCLUDED.is_final, filled_native = EXCLUDED.filled_native, avg_fill_price = EXCLUDED.avg_fill_price, \
             last_error = EXCLUDED.last_error, submitted_at = EXCLUDED.submitted_at, acked_at = EXCLUDED.acked_at, \
             updated_at = EXCLUDED.updated_at, submit_to_ack_ms = EXCLUDED.submit_to_ack_ms, \
             order_json = EXCLUDED.order_json, last_venue_json = EXCLUDED.last_venue_json",
        )
        .bind(o.id)
        .bind(o.route_id)
        .bind(o.leg_index)
        .bind(format!("{:?}", o.mode))
        .bind(o.platform.as_str())
        .bind(&o.network)
        .bind(&o.native_id)
        .bind(format!("{:?}", o.side))
        .bind(o.qty_native.to_string())
        .bind(o.limit_price_native.to_string())
        .bind(o.salt.to_string())
        .bind(o.expected_avg_per_underlying.to_string())
        .bind(o.expected_total.to_string())
        .bind(o.expected_fee.map(|d| d.to_string()))
        .bind(&o.precomputed_venue_id)
        .bind(&o.venue_order_id)
        .bind(format!("{:?}", o.state))
        .bind(o.is_final)
        .bind(o.filled_native.to_string())
        .bind(o.avg_fill_price_native.map(|d| d.to_string()))
        .bind(&o.last_error)
        .bind(o.created_at)
        .bind(o.submitted_at)
        .bind(o.acked_at)
        .bind(o.updated_at)
        .bind(o.submit_to_ack_ms)
        .bind(to_json(o)?)
        .bind(&o.last_venue_response)
        .bind(o.owner())
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO order_events (order_id, state, detail) VALUES ($1,$2,$3)")
            .bind(o.id)
            .bind(event)
            .bind(detail)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_order(&self, id: Uuid) -> Result<Option<OrderRow>, StoreError> {
        let r = sqlx::query("SELECT order_json FROM exec_orders WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        r.map(|r| from_json(r.get::<Value, _>("order_json")))
            .transpose()
    }

    async fn list_orders(&self, limit: i64) -> Result<Vec<OrderRow>, StoreError> {
        let rows =
            sqlx::query("SELECT order_json FROM exec_orders ORDER BY created_at DESC LIMIT $1")
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|r| from_json(r.get::<Value, _>("order_json")))
            .collect()
    }

    async fn list_orders_by_owner(
        &self,
        owner: &str,
        limit: i64,
    ) -> Result<Vec<OrderRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT order_json FROM exec_orders WHERE owner = $1 AND submitted_at IS NOT NULL \
             ORDER BY created_at DESC LIMIT $2",
        )
        .bind(owner.to_lowercase())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| from_json(r.get::<Value, _>("order_json")))
            .collect()
    }

    async fn open_orders(&self) -> Result<Vec<OrderRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT order_json FROM exec_orders WHERE NOT is_final ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| from_json(r.get::<Value, _>("order_json")))
            .collect()
    }
    async fn upsert_integrator(
        &self,
        id: Uuid,
        key_hash: &str,
        record: &Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO integrators (id, key_hash, record, updated_at) VALUES ($1, $2, $3, now())
             ON CONFLICT (id) DO UPDATE SET record = EXCLUDED.record, updated_at = now()",
        )
        .bind(id)
        .bind(key_hash)
        .bind(record)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn save_prepared(
        &self,
        route_id: Uuid,
        mode: &str,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        payload: &Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO prepared_executions (route_id, mode, created_at, expires_at, payload) \
             VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(route_id)
        .bind(mode)
        .bind(created_at)
        .bind(expires_at)
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_prepared(
        &self,
        route_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<Value>, StoreError> {
        let r = sqlx::query(
            "SELECT payload FROM prepared_executions \
             WHERE route_id = $1 AND taken_at IS NULL AND expires_at > $2",
        )
        .bind(route_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(r.map(|r| r.get::<Value, _>("payload")))
    }

    async fn take_prepared(&self, route_id: Uuid, now: DateTime<Utc>) -> Result<bool, StoreError> {
        let r = sqlx::query(
            "UPDATE prepared_executions SET taken_at = $2 \
             WHERE route_id = $1 AND taken_at IS NULL AND expires_at > $2",
        )
        .bind(route_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    async fn purge(
        &self,
        expired_before: DateTime<Utc>,
        simulations_before: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        let mut n = 0;
        for (sql, at) in [
            (
                "DELETE FROM route_quotes WHERE expires_at < $1",
                expired_before,
            ),
            (
                "DELETE FROM prepared_executions WHERE expires_at < $1",
                expired_before,
            ),
            (
                "DELETE FROM simulations WHERE created_at < $1",
                simulations_before,
            ),
        ] {
            n += sqlx::query(sql)
                .bind(at)
                .execute(&self.pool)
                .await?
                .rows_affected();
        }
        Ok(n)
    }

    async fn committed_spend_since(
        &self,
        mode: &str,
        since: DateTime<Utc>,
    ) -> Result<rust_decimal::Decimal, StoreError> {
        // Amounts are stored as exact decimal TEXT: summed as NUMERIC, read back as TEXT.
        let r = sqlx::query(
            "SELECT COALESCE(SUM(limit_price::numeric * qty_native::numeric \
                                 + COALESCE(expected_fee, '0')::numeric), 0)::text AS spend \
             FROM exec_orders \
             WHERE mode = $1 AND created_at >= $2 AND side = 'Buy' AND state <> 'Validated' \
               AND NOT (is_final AND filled_native::numeric = 0)",
        )
        .bind(mode)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        r.get::<String, _>("spend")
            .parse()
            .map_err(|e| StoreError(format!("spend: {e}")))
    }

    async fn load_integrators(&self) -> Result<Vec<(String, Value)>, StoreError> {
        let rows = sqlx::query("SELECT key_hash, record FROM integrators")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("key_hash"), r.get::<Value, _>("record")))
            .collect())
    }
}

/// Test-only in-memory store.
#[derive(Default)]
pub struct MemStore {
    pub simulations: Mutex<Vec<(Uuid, Value)>>,
    pub quotes: Mutex<usize>,
    pub audits: Mutex<Vec<String>>,
    pub routes: Mutex<Vec<(StoredRoute, Option<DateTime<Utc>>)>>,
    pub orders: Mutex<Vec<OrderRow>>,
    pub events: Mutex<Vec<(Uuid, String)>>,
    pub integrators: Mutex<Vec<(Uuid, String, Value)>>,
    pub prepared: Mutex<std::collections::HashMap<Uuid, MemPrepared>>,
}

/// (expires_at, taken, payload)
type MemPrepared = (DateTime<Utc>, bool, Value);

#[async_trait]
impl Store for MemStore {
    async fn ping(&self) -> Result<(), StoreError> {
        Ok(())
    }
    async fn record_quotes(
        &self,
        _: Uuid,
        _: DateTime<Utc>,
        q: &[QuoteEstimate],
    ) -> Result<(), StoreError> {
        *self.quotes.lock().unwrap() += q.len();
        Ok(())
    }
    async fn save_simulation(
        &self,
        id: Uuid,
        _: DateTime<Utc>,
        _: &str,
        _: Option<&str>,
        _: &Value,
        record: &Value,
    ) -> Result<(), StoreError> {
        self.simulations.lock().unwrap().push((id, record.clone()));
        Ok(())
    }
    async fn get_simulation(&self, id: Uuid) -> Result<Option<Value>, StoreError> {
        Ok(self
            .simulations
            .lock()
            .unwrap()
            .iter()
            .find(|(i, _)| *i == id)
            .map(|x| x.1.clone()))
    }
    async fn audit(&self, action: &str, _: Option<&str>, _: &Value) -> Result<(), StoreError> {
        self.audits.lock().unwrap().push(action.into());
        Ok(())
    }
    fn kind(&self) -> &'static str {
        "memory (tests only)"
    }

    async fn save_route(&self, r: &StoredRoute) -> Result<(), StoreError> {
        self.routes.lock().unwrap().push((r.clone(), None));
        Ok(())
    }
    async fn consume_route(
        &self,
        id: Uuid,
        hash: &str,
        now: DateTime<Utc>,
    ) -> Result<ConsumeOutcome, StoreError> {
        let mut rs = self.routes.lock().unwrap();
        let Some((r, consumed)) = rs.iter_mut().find(|(r, _)| r.id == id) else {
            return Ok(ConsumeOutcome::NotFound);
        };
        Ok(if consumed.is_some() {
            ConsumeOutcome::AlreadyConsumed
        } else if r.route_hash != hash {
            ConsumeOutcome::HashMismatch
        } else if now >= r.expires_at {
            ConsumeOutcome::Expired
        } else {
            *consumed = Some(now);
            ConsumeOutcome::Consumed(Box::new(r.clone()))
        })
    }
    async fn upsert_order(&self, o: &OrderRow, event: &str, _: &Value) -> Result<(), StoreError> {
        let mut os = self.orders.lock().unwrap();
        match os.iter_mut().find(|x| x.id == o.id) {
            Some(x) => *x = o.clone(),
            None => os.push(o.clone()),
        }
        self.events.lock().unwrap().push((o.id, event.into()));
        Ok(())
    }
    async fn get_order(&self, id: Uuid) -> Result<Option<OrderRow>, StoreError> {
        Ok(self
            .orders
            .lock()
            .unwrap()
            .iter()
            .find(|o| o.id == id)
            .cloned())
    }
    async fn list_orders(&self, limit: i64) -> Result<Vec<OrderRow>, StoreError> {
        Ok(self
            .orders
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take(limit as usize)
            .cloned()
            .collect())
    }
    async fn list_orders_by_owner(
        &self,
        owner: &str,
        limit: i64,
    ) -> Result<Vec<OrderRow>, StoreError> {
        let owner = owner.to_lowercase();
        Ok(self
            .orders
            .lock()
            .unwrap()
            .iter()
            .rev()
            .filter(|o| o.submitted_at.is_some() && o.owner().as_deref() == Some(owner.as_str()))
            .take(limit as usize)
            .cloned()
            .collect())
    }
    async fn open_orders(&self) -> Result<Vec<OrderRow>, StoreError> {
        Ok(self
            .orders
            .lock()
            .unwrap()
            .iter()
            .filter(|o| !o.is_final)
            .cloned()
            .collect())
    }

    async fn upsert_integrator(
        &self,
        id: Uuid,
        key_hash: &str,
        record: &Value,
    ) -> Result<(), StoreError> {
        let mut g = self.integrators.lock().unwrap();
        g.retain(|(i, _, _)| *i != id);
        g.push((id, key_hash.to_string(), record.clone()));
        Ok(())
    }

    async fn save_prepared(
        &self,
        route_id: Uuid,
        _: &str,
        _: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        payload: &Value,
    ) -> Result<(), StoreError> {
        self.prepared
            .lock()
            .unwrap()
            .insert(route_id, (expires_at, false, payload.clone()));
        Ok(())
    }

    async fn get_prepared(
        &self,
        route_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<Value>, StoreError> {
        Ok(match self.prepared.lock().unwrap().get(&route_id) {
            Some((exp, false, v)) if now < *exp => Some(v.clone()),
            _ => None,
        })
    }

    async fn take_prepared(&self, route_id: Uuid, now: DateTime<Utc>) -> Result<bool, StoreError> {
        Ok(match self.prepared.lock().unwrap().get_mut(&route_id) {
            Some((exp, taken, _)) if !*taken && now < *exp => {
                *taken = true;
                true
            }
            _ => false,
        })
    }

    async fn committed_spend_since(
        &self,
        mode: &str,
        since: DateTime<Utc>,
    ) -> Result<rust_decimal::Decimal, StoreError> {
        Ok(self
            .orders
            .lock()
            .unwrap()
            .iter()
            .filter(|o| {
                format!("{:?}", o.mode) == mode
                    && o.created_at >= since
                    && o.side == oa_core::domain::Side::Buy
                    && o.state != crate::execution::OrderState::Validated
                    && !(o.is_final && o.filled_native.is_zero())
            })
            .map(|o| o.limit_price_native * o.qty_native + o.expected_fee.unwrap_or_default())
            .sum())
    }

    async fn load_integrators(&self) -> Result<Vec<(String, Value)>, StoreError> {
        Ok(self
            .integrators
            .lock()
            .unwrap()
            .iter()
            .map(|(_, h, v)| (h.clone(), v.clone()))
            .collect())
    }
}

#[cfg(test)]
mod pg_tests {
    use super::*;

    /// Runs against a real PostgreSQL when OA_TEST_DATABASE_URL is set (skipped otherwise).
    /// Applies the migrations, which are non-destructive.
    #[tokio::test]
    async fn prepared_executions_and_committed_spend_on_postgres() {
        let Ok(url) = std::env::var("OA_TEST_DATABASE_URL") else {
            eprintln!("OA_TEST_DATABASE_URL not set: skipped");
            return;
        };
        let s = PgStore::connect(&url).await.unwrap();
        let id = Uuid::new_v4();
        let now = Utc::now();
        let payload = serde_json::json!({"probe": id.to_string()});
        s.save_prepared(
            id,
            "PgProbe",
            now,
            now + chrono::Duration::seconds(60),
            &payload,
        )
        .await
        .unwrap();
        assert_eq!(s.get_prepared(id, now).await.unwrap(), Some(payload));
        assert!(s.take_prepared(id, now).await.unwrap());
        assert!(!s.take_prepared(id, now).await.unwrap(), "single use");
        assert_eq!(s.get_prepared(id, now).await.unwrap(), None);
        let expired = Uuid::new_v4();
        s.save_prepared(expired, "PgProbe", now, now, &Value::Null)
            .await
            .unwrap();
        assert!(!s.take_prepared(expired, now).await.unwrap(), "expired");
        // A mode no order uses: zero, parsed from NUMERIC text.
        let spend = s
            .committed_spend_since("PgProbe", now - chrono::Duration::hours(24))
            .await
            .unwrap();
        assert!(spend.is_zero());
        // The real modes parse too (whatever their journal holds).
        s.committed_spend_since("Testnet", now - chrono::Duration::days(3650))
            .await
            .unwrap();
        // Owner column + index exist (0006); an address nobody used owns nothing.
        let none = s
            .list_orders_by_owner("0x000000000000000000000000000000000000dEaD", 10)
            .await
            .unwrap();
        assert!(none.is_empty());
        // Retention: an execution prepared two days ago is purged; simulations are left alone here.
        let old = Uuid::new_v4();
        let then = now - chrono::Duration::days(2);
        s.save_prepared(old, "PgProbe", then, then, &Value::Null)
            .await
            .unwrap();
        let n = s
            .purge(
                now - chrono::Duration::days(1),
                now - chrono::Duration::days(36_500),
            )
            .await
            .unwrap();
        assert!(n >= 1, "purged {n}");
    }
}
