//! Execution contract (separate from the read-only `Connector`). Only TESTNET implementations
//! exist in this codebase; there is no mainnet execution venue.

use crate::Result;
use async_trait::async_trait;
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimeInForce {
    /// Fill what is possible immediately, cancel the rest (no resting order).
    Ioc,
    Gtc,
}

/// A fully specified order, ready to be signed. Prices are per NATIVE contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOrder {
    pub native_id: String,
    /// Venue numeric instrument id when the venue needs one (Aevo).
    pub venue_instrument_id: Option<u64>,
    /// Venue-specific signing metadata (Derive: base_asset_address, base_asset_sub_id).
    #[serde(default)]
    pub venue_meta: serde_json::Value,
    /// Max fee per unit of volume the order may pay (Derive signs it).
    #[serde(default)]
    pub max_fee_per_unit: Option<Decimal>,
    pub side: Side,
    pub qty_native: Decimal,
    pub limit_price_native: Decimal,
    /// Unique per order; part of the signed payload (anti-replay + deterministic id).
    pub salt: u64,
    /// Unix seconds, part of the signed payload.
    pub timestamp: i64,
    pub time_in_force: TimeInForce,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VenueStatus {
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    /// A status string we do not know: never treated as final.
    Unknown(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueOrderState {
    pub venue_order_id: String,
    pub status: VenueStatus,
    pub filled_native: Decimal,
    pub avg_price_native: Option<Decimal>,
    /// True when the venue will not change this order any more (filled, cancelled, rejected,
    /// or an IOC partial whose remainder was cancelled).
    pub is_final: bool,
    /// Raw venue response (no secrets: responses never contain our keys).
    pub raw: serde_json::Value,
}

#[async_trait]
pub trait ExecutionVenue: Send + Sync {
    fn platform(&self) -> PlatformId;
    /// Network label; must match the instruments' network (e.g. "aevo-testnet").
    fn network(&self) -> &'static str;
    /// Venue order id computable BEFORE submission (if the venue derives it from the signed
    /// payload). Lets a lost acknowledgement be reconciled by lookup instead of resubmission.
    fn precompute_order_id(&self, order: &ExecOrder) -> Option<String>;
    async fn submit(&self, order: &ExecOrder) -> Result<VenueOrderState>;
    /// `Ok(None)` = the venue says this order does not exist.
    async fn get_order(&self, venue_order_id: &str) -> Result<Option<VenueOrderState>>;
    async fn cancel(&self, venue_order_id: &str) -> Result<VenueOrderState>;
    /// Non-secret identity for diagnostics (e.g. maker address, signing-key address).
    fn identity(&self) -> serde_json::Value;
    /// Real account balance on this venue (authenticated read).
    async fn balance(&self) -> Result<VenueBalance>;

    /// Transaction venues whose ONE transaction carries several legs (CallPut spreads): the shared
    /// unsigned transaction for these orders. Default: not supported.
    async fn prepare_user_tx_bundle(
        &self,
        _orders: &[ExecOrder],
    ) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }
    /// Non-custodial TRANSACTION venues (the user's wallet sends the tx): the unsigned transaction
    /// for this order. `None` for venues that use signed messages or our own keys.
    async fn prepare_user_tx(&self, _order: &ExecOrder) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }
    /// Non-custodial venues that can check the user's signatures locally: called BEFORE the
    /// prepared execution is consumed, so a forged submission cannot burn it. Default: checked at
    /// submit only.
    fn check_user_signatures(&self, _order: &ExecOrder) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueBalance {
    /// Spendable now (premium + fees), in `currency`.
    pub available: Decimal,
    pub equity: Option<Decimal>,
    pub currency: String,
    pub raw: serde_json::Value,
}
