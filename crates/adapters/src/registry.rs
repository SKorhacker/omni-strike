//! Inventory of ALL platforms in scope, with the research verdicts of 2026-10-05. Every platform
//! appears in the product, connected or not, with what is missing.

use oa_core::domain::{
    Capability, CapabilityStatus, IntegrationLevel, OperationalStatus, PlatformId, ProductFamily,
};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct PlatformInfo {
    pub id: PlatformId,
    pub display_name: &'static str,
    pub aliases: &'static [&'static str],
    pub families: &'static [ProductFamily],
    pub networks: &'static [&'static str],
    pub operational_status: OperationalStatus,
    pub integration_level: IntegrationLevel,
    /// What the PLATFORM offers (not what we implemented).
    pub capabilities: Vec<(Capability, CapabilityStatus)>,
    pub official_sources: &'static [&'static str],
    pub verified_on: &'static str,
    /// What is missing / why it is not integrated.
    pub notes: &'static str,
}

use Capability as C;
use CapabilityStatus::{Supported as S, Unsupported as U, Unverified as V};

fn caps(list: [(Capability, CapabilityStatus); 13]) -> Vec<(Capability, CapabilityStatus)> {
    debug_assert!(list.iter().map(|x| x.0).eq(Capability::ALL));
    list.to_vec()
}

#[rustfmt::skip]
pub fn all() -> Vec<PlatformInfo> {
    vec![
        PlatformInfo {
            id: PlatformId::Derive, display_name: "Derive", aliases: &["Lyra", "Lyra Finance"],
            families: &[ProductFamily::DatedOption],
            networks: &["Ethereum L1 (v3, zk app, chainId 1)", "Ethereum Sepolia (v3 testnet)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,V),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,S),(C::ExistingMcp,U)]),
            official_sources: &["https://www.derive.xyz/", "https://docs.derive.xyz/", "https://legacy.docs.derive.xyz"],
            verified_on: "2026-10-09",
            notes: "v3 MAINNET live (replaced v2: v2 WS answers 503 since 2026-10-07; accounts snapshotted into v3). Read: v3 catalog + WS books + slim ticker [LIVE 2026-10-09, 628 BTC / 798 ETH options]. Execution: non-custodial (user signs the EIP-712 Action), testnet orders FILLED; mainnet signing domain 0xda61...fe19 verified, Action.data accepted by Derive's public/decode_action on mainnet; mainnet orders only in the mainnet-nc build.",
        },
        PlatformInfo {
            id: PlatformId::Aevo, display_name: "Aevo", aliases: &["Ribbon"],
            families: &[ProductFamily::DatedOption],
            networks: &["Aevo L2 (EVM rollup)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,V),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,S),(C::ExistingMcp,S)]),
            official_sources: &["https://www.aevo.xyz/", "https://api-docs.aevo.xyz/"],
            verified_on: "2026-10-05",
            notes: "Integrated: public REST read-only (markets, orderbook snapshot, instrument). WS deltas not used (no sequence number; checksum not verified by us). Settlement window undocumented.",
        },
        PlatformInfo {
            id: PlatformId::Paradex, display_name: "Paradex", aliases: &[],
            families: &[ProductFamily::DatedOption],
            networks: &["Paradex (Starknet app-chain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,V),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,S),(C::ExistingMcp,S)]),
            official_sources: &["https://docs.paradex.trade/"],
            verified_on: "2026-10-05",
            notes: "Integrated read-only (catalog, STANDARD API book, summary, per-market API fees). Retail-only RPI orders are never used: on 2026-10-05 the standard book of the most-held BTC options was empty, so Paradex adds little API-executable depth today. Execution not integrated.",
        },
        PlatformInfo {
            id: PlatformId::Hegic, display_name: "Hegic", aliases: &["Hegic Herge (2022 upgrade)"],
            families: &[ProductFamily::DatedOption],
            networks: &["Arbitrum", "Ethereum"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,U),(C::Quote,S),(C::Balances,V),(C::Positions,S),(C::Simulation,V),(C::PlaceOrder,S),(C::CancelOrder,U),(C::OrderStatus,V),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://www.hegic.co/"],
            verified_on: "2026-10-05",
            notes: "Integrated read-only (2026-10-08): on-chain PriceCalculator.calculatePremium (Arbitrum) for the expiries/strikes other venues list (Fridays 08:00 UTC, 7-90 days, strikes near spot). American, cash-settled USDC, ask only. INDICATIVE: buy() takes no strike (fixed moneyness, strike = spot at purchase), so quotes are shown but never routed. ~$57k/day.",
        },
        PlatformInfo {
            id: PlatformId::Stryke, display_name: "Stryke", aliases: &["Dopex"],
            families: &[ProductFamily::DatedOption],
            networks: &["Arbitrum", "Mantle", "Base", "Sonic", "Blast"],
            operational_status: OperationalStatus::Legacy,
            integration_level: IntegrationLevel::DocumentedOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,U),(C::Quote,S),(C::Balances,V),(C::Positions,S),(C::Simulation,S),(C::PlaceOrder,S),(C::CancelOrder,U),(C::OrderStatus,V),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,U)]),
            official_sources: &["https://docs.stryke.xyz/"],
            verified_on: "2026-10-05",
            notes: "Official docs label the CLAMM API 'deprecated'; every market showed volume24h = 0 on 2026-10-05 (single observation).",
        },
        PlatformInfo {
            id: PlatformId::Panoptic, display_name: "Panoptic", aliases: &[],
            families: &[ProductFamily::PerpetualOption],
            networks: &["Ethereum", "Robinhood Chain"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::DocumentedOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,U),(C::Quote,U),(C::Balances,S),(C::Positions,S),(C::Simulation,S),(C::PlaceOrder,S),(C::CancelOrder,U),(C::OrderStatus,S),(C::Settlement,U),(C::TestEnvironment,U),(C::ExistingMcp,V)]),
            official_sources: &["https://panoptic.xyz/", "https://panoptic.xyz/docs"],
            verified_on: "2026-10-05",
            notes: "Perpetual options with streamed premium: no fixed premium for a size, never ranked against dated options.",
        },
        PlatformInfo {
            id: PlatformId::Premia, display_name: "Premia / Kyan", aliases: &["Premia Blue (V3)", "Kyan (Premia V4)"],
            families: &[ProductFamily::DatedOption],
            networks: &["Arbitrum"],
            operational_status: OperationalStatus::Unverified,
            integration_level: IntegrationLevel::Blocked,
            capabilities: caps([(C::InstrumentDiscovery,V),(C::MarketData,V),(C::Orderbook,V),(C::Quote,V),(C::Balances,V),(C::Positions,V),(C::Simulation,V),(C::PlaceOrder,V),(C::CancelOrder,V),(C::OrderStatus,V),(C::Settlement,V),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://premia.blue/", "https://docs.premia.blue/", "https://docs.kyan.blue/"],
            verified_on: "2026-10-05",
            notes: "Premia Blue orderbook API answered 503 'Service is disabled'; Kyan API host did not resolve (DNS) on 2026-10-05. Point-in-time observations, cause unknown: not proof of closure.",
        },
        PlatformInfo {
            id: PlatformId::Thales, display_name: "Overtime Speed Markets", aliases: &["Thales", "Thales Speed Markets"],
            families: &[ProductFamily::Binary],
            networks: &["Optimism", "Arbitrum", "Base"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::Blocked,
            capabilities: caps([(C::InstrumentDiscovery,V),(C::MarketData,V),(C::Orderbook,U),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,V),(C::PlaceOrder,S),(C::CancelOrder,U),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,S),(C::ExistingMcp,V)]),
            official_sources: &["https://docs.overtime.io/", "https://docs.thales.io/"],
            verified_on: "2026-10-05",
            notes: "Binary family. Documented API host does not resolve; current API requires a key (401). Key access procedure unverified.",
        },
        PlatformInfo {
            id: PlatformId::StrikeOnchain, display_name: "STRIKE (strikeonchain.com)", aliases: &[],
            families: &[ProductFamily::Binary],
            networks: &["Base"],
            operational_status: OperationalStatus::Unavailable,
            integration_level: IntegrationLevel::Blocked,
            capabilities: caps([(C::InstrumentDiscovery,V),(C::MarketData,V),(C::Orderbook,U),(C::Quote,V),(C::Balances,V),(C::Positions,V),(C::Simulation,V),(C::PlaceOrder,V),(C::CancelOrder,V),(C::OrderStatus,V),(C::Settlement,V),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://strikeonchain.com/"],
            verified_on: "2026-10-05",
            notes: "Distinct from Stryke. Site states 'Demo · Simulated — no real funds' and that live trading launches later (official site text).",
        },
        PlatformInfo {
            id: PlatformId::Rysk, display_name: "Rysk", aliases: &["Rysk V12", "Rysk Finance"],
            families: &[ProductFamily::DatedOption],
            networks: &["HyperEVM", "Ethereum"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,V),(C::MarketData,V),(C::Orderbook,U),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,V),(C::PlaceOrder,S),(C::CancelOrder,U),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,S),(C::ExistingMcp,V)]),
            official_sources: &["https://docs.rysk.finance/"],
            verified_on: "2026-10-05",
            notes: "Integrated read-only (2026-10-08): public inventory -> one INDICATIVE bid per contract from the APY (users sell covered calls / cash-secured puts, physically settled). Shown, never routed: firm prices need the wallet taker RFQ (not integrated). ~$2M/day notional.",
        },
        PlatformInfo {
            id: PlatformId::Myso, display_name: "MYSO", aliases: &["MYSO v3"],
            families: &[ProductFamily::YieldStrategy],
            networks: &["Ethereum"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::DocumentedOnly,
            capabilities: caps([(C::InstrumentDiscovery,U),(C::MarketData,S),(C::Orderbook,U),(C::Quote,U),(C::Balances,S),(C::Positions,S),(C::Simulation,V),(C::PlaceOrder,S),(C::CancelOrder,V),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,S),(C::ExistingMcp,V)]),
            official_sources: &["https://myso.finance/", "https://myso-finance.gitbook.io/myso-v3"],
            verified_on: "2026-10-05",
            notes: "Bespoke OTC covered calls; no listed instruments, no programmatic quotes. Public GraphQL gives history only.",
        },
        PlatformInfo {
            id: PlatformId::Opyn, display_name: "Opyn", aliases: &["Squeeth", "Gamma Protocol"],
            families: &[ProductFamily::Other],
            networks: &["Ethereum"],
            operational_status: OperationalStatus::Legacy,
            integration_level: IntegrationLevel::Blocked,
            capabilities: caps([(C::InstrumentDiscovery,U),(C::MarketData,U),(C::Orderbook,U),(C::Quote,U),(C::Balances,V),(C::Positions,V),(C::Simulation,U),(C::PlaceOrder,U),(C::CancelOrder,U),(C::OrderStatus,U),(C::Settlement,V),(C::TestEnvironment,U),(C::ExistingMcp,V)]),
            official_sources: &["https://opyn.co", "https://opyn.gitbook.io/opyn"],
            verified_on: "2026-10-05",
            notes: "Squeeth controller isShutDown() returned true (on-chain read; address from model knowledge). Gamma options legacy. New product is perps.",
        },
        PlatformInfo {
            id: PlatformId::Thetanuts, display_name: "Thetanuts Finance V4", aliases: &["Odette", "Thetanuts"],
            families: &[ProductFamily::DatedOption],
            networks: &["Base"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,S),(C::PlaceOrder,S),(C::CancelOrder,U),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,U),(C::ExistingMcp,S)]),
            official_sources: &["https://www.npmjs.com/package/@thetanuts-finance/thetanuts-client", "https://thetanuts.finance/"],
            verified_on: "2026-10-06",
            notes: "Integrated read-only: maker-signed OptionBook orders (single strike, maker short, USDC collateral) as ask levels; fees from on-chain calculateFee; 50 USDC minimum premium. Execution = on-chain fillOrder on Base (not integrated); no testnet. Small sizes (~10k USDC per order), 1-3 day expiries.",
        },
        PlatformInfo {
            id: PlatformId::Deribit, display_name: "Deribit (USDC linear options)", aliases: &["Deribit"],
            families: &[ProductFamily::DatedOption],
            networks: &["Centralised exchange (not onchain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,S),(C::ExistingMcp,S)]),
            official_sources: &["https://docs.deribit.com/", "https://www.deribit.com/kb/fees"],
            verified_on: "2026-10-06",
            notes: "CEX reference venue added for the cost goal. Integrated read-only: USDC-settled linear BTC/ETH options (public REST). Inverse coin-settled options not used. Trading needs the user's own account (KYC) + API keys; test.deribit.com testnet exists. Not available in some jurisdictions.",
        },
        PlatformInfo {
            id: PlatformId::Okx, display_name: "OKX options", aliases: &["OKEx"],
            families: &[ProductFamily::DatedOption],
            networks: &["Centralised exchange (not onchain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://www.okx.com/docs-v5/en/", "https://www.okx.com/en-us/help/trading-fee-rules-faq"],
            verified_on: "2026-10-06",
            notes: "CEX. Read-only: coin-margined BTC/ETH options (premium+settlement in coin; levels converted to USD at the OKX index, flagged; listed first) and USD-margined _UM (UAE + Australia wholesale only, thin). European, cash, 08:00 UTC. Fees Lv1 0.03% capped 7% of premium. Needs the user's own OKX account (KYC) + API keys.",
        },
        PlatformInfo {
            id: PlatformId::Bybit, display_name: "Bybit options (USDT)", aliases: &["Bybit"],
            families: &[ProductFamily::DatedOption],
            networks: &["Centralised exchange (not onchain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://bybit-exchange.github.io/docs/v5/intro", "https://www.bybit.com/en/help-center/article?language=en_US&id=000001544"],
            verified_on: "2026-10-06",
            notes: "CEX. Read-only: BTC/ETH European options, premium+settlement USDT (converted to USDC at the live Kraken rate, settlement asset flagged). 30-min index average before 08:00 UTC. Fees taker 0.03%/maker 0.02% of index, cap 7% of premium. Needs own account (KYC).",
        },
        PlatformInfo {
            id: PlatformId::Binance, display_name: "Binance European options (USDT)", aliases: &["Binance Options"],
            families: &[ProductFamily::DatedOption],
            networks: &["Centralised exchange (not onchain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://developers.binance.com/docs/derivatives/option/", "https://www.binance.com/en/fee/optionsTrading"],
            verified_on: "2026-10-06",
            notes: "CEX. Read-only: BTC/ETH European options, premium/collateral/settlement USDT (converted to USDC at the live Kraken rate, flagged). Fees 0.024% of index capped 10% of premium; exercise fee not modelled. Needs own account (KYC).",
        },
        PlatformInfo {
            id: PlatformId::Bullish, display_name: "Bullish options", aliases: &["Bullish"],
            families: &[ProductFamily::DatedOption],
            networks: &["Centralised exchange (not onchain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://docs.exchange.bullish.com/", "https://www.bullish.com/global-derivatives"],
            verified_on: "2026-10-06",
            notes: "CEX. Read-only: USDC-settled European BTC options (no ETH). Fee = documented 10% premium cap used as an UPPER BOUND (rate not public). Low on-screen volume. Needs own account (KYC).",
        },
        PlatformInfo {
            id: PlatformId::Coincall, display_name: "Coincall options", aliases: &["Coincall"],
            families: &[ProductFamily::DatedOption],
            networks: &["Centralised exchange (not onchain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://docs.coincall.com/"],
            verified_on: "2026-10-06",
            notes: "CEX. Read-only: BTC/ETH options, premium unit 'USD' as reported (stablecoin not confirmed -> PREMIUM_CURRENCY_DIFFERS vs USDC). Fee = 0.03% of notional, uncapped UPPER BOUND. Exercise style not verified.",
        },
        PlatformInfo {
            id: PlatformId::Gate, display_name: "Gate options (USDT)", aliases: &["Gate.io"],
            families: &[ProductFamily::DatedOption],
            networks: &["Centralised exchange (not onchain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://www.gate.io/docs/developers/apiv4/", "https://www.gate.com/help/other/optionstradingrules/45829"],
            verified_on: "2026-10-06",
            notes: "CEX. Read-only: USDT-settled European BTC/ETH options, 0.01 coin per contract (converted to USDC at the live Kraken rate, flagged). Fee = min(0.03% of notional, 12.5% of premium); exercise fee not modelled.",
        },
        PlatformInfo {
            id: PlatformId::Delta, display_name: "Delta Exchange options (global, USDT)", aliases: &["Delta Exchange"],
            families: &[ProductFamily::DatedOption],
            networks: &["Centralised exchange (not onchain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://docs.delta.exchange/"],
            verified_on: "2026-10-06",
            notes: "CEX. Read-only: USDT-settled BTC/ETH options, settle 12:00 UTC (converted to USDC, flagged). Fee = min(0.015% of notional, 7.5% of premium). Thin liquidity. India venue excluded. Exercise style not verified.",
        },
        PlatformInfo {
            id: PlatformId::Kalshi, display_name: "Kalshi (crypto threshold binaries)", aliases: &["Kalshi"],
            families: &[ProductFamily::Binary],
            networks: &["Centralised exchange (CFTC-regulated DCM, not onchain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://docs.kalshi.com/", "https://kalshi.com/docs/kalshi-fee-schedule.pdf"],
            verified_on: "2026-10-06",
            notes: "Integrated read-only: KXBTCD/KXETHD 'above' binaries (YES if the BRTI/ERTI 60 s average is strictly above floor_strike at close_time UTC), public REST, bulk tops. Quadratic taker fee 0.07 C P(1-P) rounded up to the cent; USD (not USDC). Trading requires a verified Kalshi account (US persons, KYC).",
        },
        PlatformInfo {
            id: PlatformId::Hyperliquid, display_name: "Hyperliquid HIP-4 outcome markets", aliases: &["HIP-4"],
            families: &[ProductFamily::Binary],
            networks: &["Hyperliquid L1"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,V),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &["https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-4-outcome-markets"],
            verified_on: "2026-10-06",
            notes: "Integrated read-only: BTC/ETH threshold binaries (Recurring daily: YES if HL mark >= target at 06:00 UTC; third-party binaryPrice: strictly above, TWAP window stated per market). Quote USDC per API. Fees UNKNOWN (not published) -> never ranked. Thin books.",
        },
        PlatformInfo {
            id: PlatformId::Polymarket, display_name: "Polymarket (crypto price binaries)", aliases: &[],
            families: &[ProductFamily::Binary],
            networks: &["Polygon (CLOB off-chain)"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::Blocked,
            capabilities: caps([(C::InstrumentDiscovery,V),(C::MarketData,V),(C::Orderbook,V),(C::Quote,V),(C::Balances,V),(C::Positions,V),(C::Simulation,V),(C::PlaceOrder,V),(C::CancelOrder,V),(C::OrderStatus,V),(C::Settlement,V),(C::TestEnvironment,V),(C::ExistingMcp,V)]),
            official_sources: &[],
            verified_on: "2026-10-06",
            notes: "Geo-blocked in several jurisdictions (the US, France where the regulator ANJ blocks the domain, and others): not integrated, legal blocks are never circumvented. Data observed 2026-10-06 (research only): daily BTC 'above' markets ~$650k/24h, fee 0.07 p(1-p) taker.",
        },
        PlatformInfo {
            id: PlatformId::Callput, display_name: "CallPut", aliases: &["callput.app"],
            families: &[ProductFamily::DatedOption],
            networks: &["Base"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,S),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,U),(C::ExistingMcp,S)]),
            official_sources: &["https://docs.callput.app/developers/contracts-addresses-and-data-sources", "https://docs.callput.app/traders/fees-limits-and-faq"],
            verified_on: "2026-10-08",
            notes: "Integrated read-only: public pool feed (crypto + synthetic stock/ETF options: SPY, QQQ, NVDA, TSLA, SPCX...). Price = mark x (1 +/- risk premium); size bounded by the pool's available USDC. Execution = keeper-executed requests on Base (not integrated). ~$7-9M/day notional from ~100 traders.",
        },
        PlatformInfo {
            id: PlatformId::Hypercall, display_name: "Hypercall", aliases: &[],
            families: &[ProductFamily::DatedOption],
            networks: &["Hyperliquid"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,U),(C::ExistingMcp,U)]),
            official_sources: &["https://docs.hypercall.xyz/", "https://api.hypercall.xyz/instruments"],
            verified_on: "2026-10-08",
            notes: "Options on Hyperliquid (crypto + SP500, NVDA, MU...). Public API; visible books empty on 40/40 contracts sampled: prices come from an RFQ / RPI auction (account required).",
        },
        PlatformInfo {
            id: PlatformId::Limitless, display_name: "Limitless (crypto price binaries)", aliases: &[],
            families: &[ProductFamily::Binary],
            networks: &["Base"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,U),(C::ExistingMcp,U)]),
            official_sources: &["https://api.limitless.exchange/markets/active"],
            verified_on: "2026-10-08",
            notes: "Binary order books (up/down and 'BTC above K' ladders). Volume under $2k/day on crypto markets (2026-10-07).",
        },
        PlatformInfo {
            id: PlatformId::Myriad, display_name: "Myriad (crypto price binaries)", aliases: &[],
            families: &[ProductFamily::Binary],
            networks: &["BNB Chain", "Abstract", "Linea"],
            operational_status: OperationalStatus::Active,
            integration_level: IntegrationLevel::VerifiedReadOnly,
            capabilities: caps([(C::InstrumentDiscovery,S),(C::MarketData,S),(C::Orderbook,S),(C::Quote,S),(C::Balances,S),(C::Positions,S),(C::Simulation,U),(C::PlaceOrder,S),(C::CancelOrder,S),(C::OrderStatus,S),(C::Settlement,S),(C::TestEnvironment,U),(C::ExistingMcp,U)]),
            official_sources: &["https://api-v2.myriadprotocol.com/events"],
            verified_on: "2026-10-08",
            notes: "Daily 'BTC above K at 16:00 UTC' ladders (BNB order book, USD1) and AMM markets (some in points). Thin.",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_platform_is_listed_once_with_all_capabilities() {
        let r = all();
        assert_eq!(r.len(), PlatformId::ALL.len());
        for p in PlatformId::ALL {
            let e: Vec<_> = r.iter().filter(|x| x.id == p).collect();
            assert_eq!(e.len(), 1, "{p}");
            assert_eq!(e[0].capabilities.len(), 13, "{p}");
        }
    }

    #[test]
    fn only_implemented_connectors_claim_verified_levels() {
        for p in all() {
            let verified = matches!(
                p.integration_level,
                IntegrationLevel::VerifiedReadOnly
                    | IntegrationLevel::VerifiedTestExecution
                    | IntegrationLevel::VerifiedLiveExecution
            );
            assert_eq!(
                verified,
                matches!(
                    p.id,
                    PlatformId::Derive
                        | PlatformId::Aevo
                        | PlatformId::Paradex
                        | PlatformId::Thetanuts
                        | PlatformId::Rysk
                        | PlatformId::Callput
                        | PlatformId::Hypercall
                        | PlatformId::Myriad
                        | PlatformId::Limitless
                        | PlatformId::Hegic
                        | PlatformId::Deribit
                        | PlatformId::Okx
                        | PlatformId::Bybit
                        | PlatformId::Binance
                        | PlatformId::Bullish
                        | PlatformId::Coincall
                        | PlatformId::Gate
                        | PlatformId::Delta
                        | PlatformId::Kalshi
                        | PlatformId::Hyperliquid
                ),
                "{}",
                p.id
            );
            assert_ne!(p.integration_level, IntegrationLevel::VerifiedLiveExecution);
            assert_ne!(p.integration_level, IntegrationLevel::VerifiedTestExecution);
        }
    }
}
