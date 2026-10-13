//! Integrators (platforms, wallets, terminals, bots that embed Omni Strike): API keys, self-service
//! settings with default limits ON, and per-key rate limiting.
//!
//! Keys: `os_<64 hex>` shown ONCE at creation; only SHA-256(key) is stored. Lookups hit an in-memory
//! map (loaded at startup, updated on write), never the database per request.
//! Settings: every integrator starts with conservative defaults and may change them itself within
//! HARD caps; values beyond the caps need an admin (us).

use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::domain::PlatformId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use uuid::Uuid;

use crate::store::{Store, StoreError};

/// Self-service limits. Defaults are deliberately prudent ("default limits ON").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegratorSettings {
    /// Max notional (underlying qty x index, USD) of one route/strategy/execution.
    #[schemars(with = "String")]
    pub max_notional_per_order_usd: Decimal,
    /// Max notional executed per UTC day.
    #[schemars(with = "String")]
    pub max_notional_per_day_usd: Decimal,
    /// Venues the integrator allows (None = all).
    pub allowed_platforms: Option<Vec<PlatformId>>,
    /// Underlyings allowed (None = all).
    pub allowed_underlyings: Option<Vec<String>>,
    /// Limit-price tolerance vs the quoted average (bps).
    pub max_slippage_bps: u32,
    /// Max cost of unwinding filled legs when another leg of a multi-venue order fails (bps of notional).
    pub unwind_cost_cap_bps: u32,
    /// Default atomicity tolerance for strategies (bps of notional).
    pub single_venue_tolerance_bps: u32,
    /// Informational: execution always requires a single-use quoted route (cannot be turned off).
    pub simulation_first: bool,
    /// Sustained requests per second (token bucket; burst = 2x).
    pub rate_limit_rps: u32,
    /// Allow strategies with UNBOUNDED loss (net short calls, naked sells). Off by default.
    #[serde(default)]
    pub allow_naked_sells: bool,
    /// Signed webhook destination for execution reports (https; http only for localhost).
    #[serde(default)]
    pub webhook_url: Option<String>,
}

impl Default for IntegratorSettings {
    fn default() -> Self {
        Self {
            max_notional_per_order_usd: Decimal::from(250_000),
            max_notional_per_day_usd: Decimal::from(2_000_000),
            allowed_platforms: None,
            allowed_underlyings: None,
            max_slippage_bps: 50,
            unwind_cost_cap_bps: 30,
            single_venue_tolerance_bps: 10,
            simulation_first: true,
            rate_limit_rps: 20,
            allow_naked_sells: false,
            webhook_url: None,
        }
    }
}

/// Self-service ceilings (above them: the operator, through the admin API).
pub struct HardCaps;
impl HardCaps {
    pub const MAX_NOTIONAL_PER_ORDER_USD: i64 = 5_000_000;
    pub const MAX_NOTIONAL_PER_DAY_USD: i64 = 50_000_000;
    pub const MAX_SLIPPAGE_BPS: u32 = 500;
    pub const MAX_UNWIND_COST_CAP_BPS: u32 = 300;
    pub const MAX_TOLERANCE_BPS: u32 = 200;
    pub const MAX_RPS: u32 = 200;
}

impl IntegratorSettings {
    pub fn validate(&self, admin: bool) -> Result<(), String> {
        if self.max_notional_per_order_usd <= Decimal::ZERO
            || self.max_notional_per_day_usd <= Decimal::ZERO
        {
            return Err("notional limits must be > 0".into());
        }
        if self.max_notional_per_order_usd > self.max_notional_per_day_usd {
            return Err("max_notional_per_order_usd cannot exceed max_notional_per_day_usd".into());
        }
        if self.rate_limit_rps == 0 {
            return Err("rate_limit_rps must be >= 1".into());
        }
        if let Some(u) = &self.webhook_url {
            crate::webhooks::validate_url(u, crate::webhooks::allow_local())?;
        }
        if admin {
            return Ok(());
        }
        let over = |what: &str, cap: String| {
            Err(format!(
                "{what} above the self-service cap {cap}: contact Omni Strike"
            ))
        };
        if self.max_notional_per_order_usd > Decimal::from(HardCaps::MAX_NOTIONAL_PER_ORDER_USD) {
            return over(
                "max_notional_per_order_usd",
                HardCaps::MAX_NOTIONAL_PER_ORDER_USD.to_string(),
            );
        }
        if self.max_notional_per_day_usd > Decimal::from(HardCaps::MAX_NOTIONAL_PER_DAY_USD) {
            return over(
                "max_notional_per_day_usd",
                HardCaps::MAX_NOTIONAL_PER_DAY_USD.to_string(),
            );
        }
        if self.max_slippage_bps > HardCaps::MAX_SLIPPAGE_BPS {
            return over("max_slippage_bps", HardCaps::MAX_SLIPPAGE_BPS.to_string());
        }
        if self.unwind_cost_cap_bps > HardCaps::MAX_UNWIND_COST_CAP_BPS {
            return over(
                "unwind_cost_cap_bps",
                HardCaps::MAX_UNWIND_COST_CAP_BPS.to_string(),
            );
        }
        if self.single_venue_tolerance_bps > HardCaps::MAX_TOLERANCE_BPS {
            return over(
                "single_venue_tolerance_bps",
                HardCaps::MAX_TOLERANCE_BPS.to_string(),
            );
        }
        if self.rate_limit_rps > HardCaps::MAX_RPS {
            return over("rate_limit_rps", HardCaps::MAX_RPS.to_string());
        }
        Ok(())
    }

    pub fn allows_underlying(&self, u: &str) -> bool {
        self.allowed_underlyings
            .as_ref()
            .is_none_or(|v| v.iter().any(|x| x.eq_ignore_ascii_case(u)))
    }

    pub fn allows_platform(&self, p: PlatformId) -> bool {
        self.allowed_platforms
            .as_ref()
            .is_none_or(|v| v.contains(&p))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Integrator {
    pub id: Uuid,
    pub name: String,
    /// First characters of the key, for display ("os_1a2b3c4d…").
    pub key_prefix: String,
    pub settings: IntegratorSettings,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// HMAC secret for webhooks. Returned once at creation; redacted ("") in every other response.
    #[serde(default)]
    pub webhook_secret: String,
}

impl Integrator {
    /// Copy safe to return in API responses (no webhook secret).
    pub fn redacted(mut self) -> Self {
        self.webhook_secret = String::new();
        self
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CreatedIntegrator {
    pub integrator: Integrator,
    /// The API key. Shown ONCE: store it now, it cannot be retrieved later.
    pub api_key: String,
}

pub fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

fn new_key() -> String {
    // Two v4 UUIDs = 244 random bits from the OS RNG (getrandom).
    format!("os_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

/// In-memory registry (+ write-through persistence) and token-bucket rate limiter.
pub const UNKNOWN_INTEGRATOR: &str = "unknown integrator";
/// Errors starting with this are storage failures (503), not bad input.
pub const PERSISTENCE_PREFIX: &str = "persistence: ";

pub struct Integrators {
    by_hash: RwLock<HashMap<String, Integrator>>,
    buckets: Mutex<HashMap<Uuid, Bucket>>,
    store: Option<Arc<dyn Store>>,
    /// Notional of each executable route quoted by an integrator (route id -> integrator, notional).
    routes: Mutex<HashMap<Uuid, (Uuid, Decimal, Instant)>>,
    /// Notional executed today (UTC) per integrator. Per process: resets on restart.
    daily: Mutex<HashMap<Uuid, (chrono::NaiveDate, Decimal)>>,
}

impl Default for Integrators {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl Integrators {
    pub fn in_memory() -> Self {
        Self {
            by_hash: RwLock::new(HashMap::new()),
            buckets: Mutex::new(HashMap::new()),
            store: None,
            routes: Mutex::default(),
            daily: Mutex::default(),
        }
    }

    /// Load every integrator from the store (startup).
    pub async fn load(store: Arc<dyn Store>) -> Result<Self, StoreError> {
        let rows = store.load_integrators().await?;
        let mut map = HashMap::new();
        for (hash, v) in rows {
            if let Ok(i) = serde_json::from_value::<Integrator>(v) {
                map.insert(hash, i);
            }
        }
        Ok(Self {
            by_hash: RwLock::new(map),
            buckets: Mutex::new(HashMap::new()),
            store: Some(store),
            routes: Mutex::default(),
            daily: Mutex::default(),
        })
    }

    pub fn authenticate(&self, key: &str) -> Option<Integrator> {
        let h = hash_key(key);
        self.by_hash
            .read()
            .expect("integrators lock")
            .get(&h)
            .filter(|i| i.active)
            .cloned()
    }

    pub fn list(&self) -> Vec<Integrator> {
        let mut v: Vec<_> = self
            .by_hash
            .read()
            .expect("integrators lock")
            .values()
            .cloned()
            .collect();
        v.sort_by_key(|i| i.created_at);
        v
    }

    async fn persist(&self, hash: &str, i: &Integrator) -> Result<(), StoreError> {
        if let Some(s) = &self.store {
            s.upsert_integrator(i.id, hash, &serde_json::to_value(i).expect("serializable"))
                .await?;
        }
        Ok(())
    }

    pub async fn create(
        &self,
        name: &str,
        settings: Option<IntegratorSettings>,
    ) -> Result<CreatedIntegrator, String> {
        let name = name.trim();
        if name.is_empty() || name.len() > 80 {
            return Err("name must be 1..=80 characters".into());
        }
        let settings = settings.unwrap_or_default();
        settings.validate(true)?;
        let key = new_key();
        let hash = hash_key(&key);
        let now = Utc::now();
        let i = Integrator {
            id: Uuid::new_v4(),
            name: name.into(),
            key_prefix: format!("{}…", &key[..11]),
            settings,
            active: true,
            created_at: now,
            updated_at: now,
            webhook_secret: crate::webhooks::new_secret(),
        };
        self.persist(&hash, &i).await.map_err(|e| e.to_string())?;
        self.by_hash
            .write()
            .expect("integrators lock")
            .insert(hash, i.clone());
        Ok(CreatedIntegrator {
            integrator: i,
            api_key: key,
        })
    }

    /// Replace the settings of `id` (self-service when `admin` is false: hard caps apply).
    pub async fn update_settings(
        &self,
        id: Uuid,
        s: IntegratorSettings,
        admin: bool,
    ) -> Result<Integrator, String> {
        s.validate(admin)?;
        self.change(id, |i| i.settings = s).await
    }

    pub async fn set_active(&self, id: Uuid, active: bool) -> Result<Integrator, String> {
        self.change(id, |i| i.active = active).await
    }

    /// Persist FIRST, then apply in memory: a database failure leaves memory and storage agreeing
    /// (before, a key could be deactivated in memory yet come back active after a restart).
    async fn change(
        &self,
        id: Uuid,
        edit: impl FnOnce(&mut Integrator),
    ) -> Result<Integrator, String> {
        let (hash, mut updated) = {
            let g = self.by_hash.read().expect("integrators lock");
            let (h, i) = g
                .iter()
                .find(|(_, i)| i.id == id)
                .ok_or(UNKNOWN_INTEGRATOR)?;
            (h.clone(), i.clone())
        };
        edit(&mut updated);
        updated.updated_at = Utc::now();
        self.persist(&hash, &updated)
            .await
            .map_err(|e| format!("{PERSISTENCE_PREFIX}{e}"))?;
        self.by_hash
            .write()
            .expect("integrators lock")
            .insert(hash, updated.clone());
        Ok(updated)
    }

    /// Remember the notional of a route this integrator quoted (charged only if executed).
    pub fn note_route(&self, route_id: Uuid, integrator: Uuid, notional: Decimal) {
        let mut g = self.routes.lock().expect("routes lock");
        g.retain(|_, (_, _, at)| at.elapsed().as_secs() < 300);
        g.insert(route_id, (integrator, notional, Instant::now()));
    }

    /// Charge an execution against `max_notional_per_day_usd` (UTC day, this process). Routes not
    /// quoted by this integrator are refused: the limit cannot be bypassed with someone else's quote.
    pub fn charge_daily(&self, route_id: Uuid, me: &Integrator) -> Result<(), String> {
        let (owner, notional, _) = self
            .routes
            .lock()
            .expect("routes lock")
            .get(&route_id)
            .copied()
            .ok_or("route not quoted with this API key (or expired): quote it again")?;
        if owner != me.id {
            return Err("route quoted by another integrator".into());
        }
        let today = Utc::now().date_naive();
        let mut g = self.daily.lock().expect("daily lock");
        let e = g.entry(me.id).or_insert((today, Decimal::ZERO));
        if e.0 != today {
            *e = (today, Decimal::ZERO);
        }
        if e.1 + notional > me.settings.max_notional_per_day_usd {
            return Err(format!(
                "daily notional {} + {} USD above your max_notional_per_day_usd {}",
                e.1.round_dp(2),
                notional.round_dp(2),
                me.settings.max_notional_per_day_usd
            ));
        }
        e.1 += notional;
        Ok(())
    }

    /// Token bucket: `rps` sustained, burst 2 x rps. Returns false when the call must be rejected.
    pub fn allow(&self, id: Uuid, rps: u32) -> bool {
        let cap = (rps.max(1) * 2) as f64;
        let mut g = self.buckets.lock().expect("buckets lock");
        let now = Instant::now();
        let b = g.entry(id).or_insert(Bucket {
            tokens: cap,
            at: now,
        });
        b.tokens = (b.tokens + now.duration_since(b.at).as_secs_f64() * rps.max(1) as f64).min(cap);
        b.at = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn daily_notional_is_charged_per_quoted_route() {
        let r = Integrators::in_memory();
        let mut me = r.create("Acme", None).await.unwrap().integrator;
        me.settings.max_notional_per_day_usd = Decimal::from(150);
        let other = r.create("Other", None).await.unwrap().integrator;
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        r.note_route(a, me.id, Decimal::from(100));
        r.note_route(b, me.id, Decimal::from(100));
        r.note_route(c, other.id, Decimal::from(1));
        assert!(r.charge_daily(a, &me).is_ok());
        assert!(
            r.charge_daily(b, &me)
                .unwrap_err()
                .contains("daily notional")
        );
        assert!(
            r.charge_daily(c, &me).is_err(),
            "another integrator's route"
        );
        assert!(r.charge_daily(Uuid::new_v4(), &me).is_err(), "never quoted");
    }

    #[tokio::test]
    async fn keys_are_hashed_shown_once_and_authenticate() {
        let r = Integrators::in_memory();
        let c = r.create("Acme Wallet", None).await.unwrap();
        assert!(c.api_key.starts_with("os_") && c.api_key.len() == 67);
        assert_eq!(r.authenticate(&c.api_key).unwrap().id, c.integrator.id);
        assert!(r.authenticate("os_wrong").is_none());
        // the listing never contains the key
        assert!(
            !serde_json::to_string(&r.list())
                .unwrap()
                .contains(&c.api_key)
        );
        r.set_active(c.integrator.id, false).await.unwrap();
        assert!(
            r.authenticate(&c.api_key).is_none(),
            "deactivated key refused"
        );
    }

    #[tokio::test]
    async fn self_service_settings_respect_hard_caps_admin_can_exceed() {
        let r = Integrators::in_memory();
        let c = r.create("Bot", None).await.unwrap();
        let mut s = IntegratorSettings {
            rate_limit_rps: 100,
            ..Default::default()
        };
        assert!(
            r.update_settings(c.integrator.id, s.clone(), false)
                .await
                .is_ok()
        );
        s.rate_limit_rps = 1000;
        let e = r
            .update_settings(c.integrator.id, s.clone(), false)
            .await
            .unwrap_err();
        assert!(e.contains("self-service cap"), "{e}");
        assert!(r.update_settings(c.integrator.id, s, true).await.is_ok());
        let bad = IntegratorSettings {
            max_notional_per_order_usd: Decimal::from(10),
            max_notional_per_day_usd: Decimal::from(5),
            ..Default::default()
        };
        assert!(r.update_settings(c.integrator.id, bad, true).await.is_err());
    }

    #[test]
    fn token_bucket_allows_burst_then_throttles() {
        let r = Integrators::in_memory();
        let id = Uuid::new_v4();
        let ok = (0..10).filter(|_| r.allow(id, 2)).count();
        assert_eq!(ok, 4, "burst = 2 x rps");
    }

    #[test]
    fn defaults_are_prudent_and_filters_work() {
        let s = IntegratorSettings::default();
        assert!(s.validate(false).is_ok());
        assert!(s.allows_underlying("sol") && s.allows_platform(PlatformId::Okx));
        let s = IntegratorSettings {
            allowed_underlyings: Some(vec!["BTC".into()]),
            allowed_platforms: Some(vec![PlatformId::Derive]),
            ..s
        };
        assert!(s.allows_underlying("btc") && !s.allows_underlying("ETH"));
        assert!(!s.allows_platform(PlatformId::Okx));
    }
}
