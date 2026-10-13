//! Canonical domain types shared by every layer.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Every platform in scope. Renamed protocols are ONE platform with aliases
/// (Lyra -> Derive, Dopex -> Stryke). STRIKE (strikeonchain.com) and Stryke are distinct.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformId {
    Derive,
    Aevo,
    Paradex,
    Hegic,
    Stryke,
    Panoptic,
    Premia,
    Thales,
    StrikeOnchain,
    Rysk,
    Myso,
    Opyn,
    Thetanuts,
    Deribit,
    Okx,
    Bybit,
    Binance,
    Bullish,
    Coincall,
    Gate,
    Delta,
    Kalshi,
    Hyperliquid,
    Polymarket,
    Callput,
    Hypercall,
    Limitless,
    Myriad,
}

impl PlatformId {
    pub const ALL: [PlatformId; 28] = [
        Self::Derive,
        Self::Aevo,
        Self::Paradex,
        Self::Hegic,
        Self::Stryke,
        Self::Panoptic,
        Self::Premia,
        Self::Thales,
        Self::StrikeOnchain,
        Self::Rysk,
        Self::Myso,
        Self::Opyn,
        Self::Thetanuts,
        Self::Deribit,
        Self::Okx,
        Self::Bybit,
        Self::Binance,
        Self::Bullish,
        Self::Coincall,
        Self::Gate,
        Self::Delta,
        Self::Kalshi,
        Self::Hyperliquid,
        Self::Polymarket,
        Self::Callput,
        Self::Hypercall,
        Self::Limitless,
        Self::Myriad,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Derive => "derive",
            Self::Aevo => "aevo",
            Self::Paradex => "paradex",
            Self::Hegic => "hegic",
            Self::Stryke => "stryke",
            Self::Panoptic => "panoptic",
            Self::Premia => "premia",
            Self::Thales => "thales",
            Self::StrikeOnchain => "strike_onchain",
            Self::Rysk => "rysk",
            Self::Myso => "myso",
            Self::Opyn => "opyn",
            Self::Thetanuts => "thetanuts",
            Self::Deribit => "deribit",
            Self::Okx => "okx",
            Self::Bybit => "bybit",
            Self::Binance => "binance",
            Self::Bullish => "bullish",
            Self::Coincall => "coincall",
            Self::Gate => "gate",
            Self::Delta => "delta",
            Self::Kalshi => "kalshi",
            Self::Hyperliquid => "hyperliquid",
            Self::Polymarket => "polymarket",
            Self::Callput => "callput",
            Self::Hypercall => "hypercall",
            Self::Limitless => "limitless",
            Self::Myriad => "myriad",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

impl fmt::Display for PlatformId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Axis A: is the product itself live?
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OperationalStatus {
    Unverified,
    Active,
    Unavailable,
    Legacy,
}

/// Axis B: how far OUR integration goes. A mock can never be `Verified*`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IntegrationLevel {
    DocumentedOnly,
    Mock,
    VerifiedReadOnly,
    VerifiedTestExecution,
    VerifiedLiveExecution,
    Blocked,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    InstrumentDiscovery,
    MarketData,
    Orderbook,
    Quote,
    Balances,
    Positions,
    Simulation,
    PlaceOrder,
    CancelOrder,
    OrderStatus,
    Settlement,
    TestEnvironment,
    ExistingMcp,
}

impl Capability {
    pub const ALL: [Capability; 13] = [
        Self::InstrumentDiscovery,
        Self::MarketData,
        Self::Orderbook,
        Self::Quote,
        Self::Balances,
        Self::Positions,
        Self::Simulation,
        Self::PlaceOrder,
        Self::CancelOrder,
        Self::OrderStatus,
        Self::Settlement,
        Self::TestEnvironment,
        Self::ExistingMcp,
    ];
}

/// Whether the PLATFORM offers a capability (per research), not whether we integrated it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CapabilityStatus {
    Supported,
    Unsupported,
    Unverified,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductFamily {
    /// Classic options with a fixed expiry.
    DatedOption,
    PerpetualOption,
    /// Binary / digital contracts (prediction markets: "will BTC be above K at T?"). Mapping:
    /// `option_kind` Call = pays if the settlement price is ABOVE `strike`, Put = BELOW; `expiry` =
    /// resolution instant (UTC); `contract_multiplier` = payout per contract in `premium_currency`
    /// (usually 1), so a request quantity is a PAYOUT amount. Ranges and "up/down vs open" markets
    /// are not representable as a fixed threshold and are skipped by connectors.
    Binary,
    /// Covered calls, vaults and other yield products.
    YieldStrategy,
    Other,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionKind {
    Call,
    Put,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExerciseStyle {
    European,
    American,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementMechanism {
    Cash,
    Physical,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// Collision-free identity: platform + network + native id.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstrumentKey {
    pub platform: PlatformId,
    /// Chain / venue environment, e.g. "derive-mainnet". Part of the identity.
    pub network: String,
    pub native_id: String,
}

impl fmt::Display for InstrumentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.platform, self.network, self.native_id)
    }
}

/// Canonical instrument. `None` means "not known" — never a silent default.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instrument {
    pub key: InstrumentKey,
    pub family: ProductFamily,
    /// Canonical underlying symbol, e.g. "BTC".
    pub underlying: String,
    pub option_kind: Option<OptionKind>,
    pub strike: Option<Decimal>,
    /// Exact expiry instant, UTC.
    pub expiry: Option<DateTime<Utc>>,
    pub exercise_style: Option<ExerciseStyle>,
    /// Units of underlying represented by ONE native contract (quantity) unit.
    pub contract_multiplier: Decimal,
    /// Currency in which the premium is quoted and paid, e.g. "USDC".
    pub premium_currency: String,
    pub collateral_assets: Vec<String>,
    pub settlement_asset: Option<String>,
    pub settlement_mechanism: Option<SettlementMechanism>,
    /// Human-readable settlement reference, e.g. "Derive BTC index, 30-min TWAP".
    pub settlement_index: Option<String>,
    pub settlement_window: Option<String>,
    pub tick_size: Option<Decimal>,
    /// Minimum / step of the NATIVE quantity.
    pub min_amount: Option<Decimal>,
    pub amount_step: Option<Decimal>,
    pub is_active: bool,
    /// Bump when the mapping rules for this platform change.
    pub spec_version: String,
    /// Native metadata kept verbatim for audit.
    pub native: serde_json::Value,
}

/// One price level in NATIVE units: price = premium per 1 native contract, size in native contracts.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Level {
    pub price: Decimal,
    pub size: Decimal,
}

/// Full orderbook snapshot (not a delta). Bids descending, asks ascending.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookSnapshot {
    pub instrument: InstrumentKey,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    /// Timestamp reported by the source, if any.
    pub source_ts: Option<DateTime<Utc>>,
    /// Our wall-clock reception time.
    pub received_at: DateTime<Utc>,
    /// Spot/index reference reported alongside (needed for notional-based fees).
    pub index_price: Option<Decimal>,
    /// Platform mark price per native contract (needed for mark-based fee caps).
    pub mark_price: Option<Decimal>,
    pub provenance: Provenance,
    /// Who can trade against these levels (default: any API order).
    #[serde(default)]
    pub access: BookAccess,
}

/// Who can actually take the displayed liquidity.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BookAccess {
    /// Standard book: an API order from this router can match it.
    #[default]
    Api,
    /// Only retail/interactive takers can match (e.g. Paradex RPI): shown, never routed.
    RetailOnly,
    /// Indicative level, not firm for this exact contract (Rysk APY without size; Hegic floating
    /// strike set at purchase): shown, never routed.
    IndicativeRfq,
}

/// Where a piece of data came from. Shown as a permanent badge in the UI.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Provenance {
    /// Fetched live from the platform's public endpoint.
    LiveData,
    /// Recorded real response replayed from disk.
    Fixture,
    /// Synthetic data. Never presented as real.
    Mock,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QuoteFirmness {
    /// Derived from public data; nothing guarantees a fill.
    Indicative,
    /// Platform-committed price for a size, valid until an expiry.
    Firm,
    /// Firm and we hold everything needed to submit (funds, auth).
    Executable,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TradingMode {
    ReadOnly,
    Simulation,
    Testnet,
    /// Mainnet execution where the END USER signs every order (or sends every transaction):
    /// the server holds no key and cannot move funds. Only in a `--features mainnet-nc` build.
    #[serde(rename = "MAINNET_NONCUSTODIAL")]
    MainnetNonCustodial,
    Live,
}

impl TradingMode {
    /// Modes in which an execution service may submit orders.
    pub fn executes(self) -> bool {
        matches!(self, Self::Testnet | Self::MainnetNonCustodial)
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub amount: Decimal,
    pub currency: String,
}

impl Money {
    pub fn new(amount: Decimal, currency: impl Into<String>) -> Self {
        Self {
            amount,
            currency: currency.into(),
        }
    }
}
