//! Runtime configuration from environment variables (the binaries load `.env`, or the file
//! named by `OA_ENV_FILE`, e.g. `.env.testnet`).
//!
//! Mode gates:
//! - READ_ONLY (default) and SIMULATION: always available.
//! - TESTNET: only in a binary built with `--features testnet`, only with a database whose name
//!   contains "testnet" (journals never mix with mainnet data). Still no order submission:
//!   connectors only read testnet market data in this version.
//! - MAINNET_NONCUSTODIAL: only in a binary built with `--features mainnet-nc`, only with a database
//!   whose name contains "mainnet_nc". Execution venues are the user-signed ones only (the server
//!   holds no key); the kill switch is ON unless OA_KILL_SWITCH=false.
//! - LIVE (custodial): always refused. There is no build that enables it (`--features live` fails
//!   to compile).

use oa_core::domain::TradingMode;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: Option<String>,
    pub mode: TradingMode,
    /// Optional bearer token for the API. Unset = no auth (loopback bind only).
    pub api_token: Option<String>,
    pub catalog_ttl: Duration,
    pub default_book_max_age: Duration,
    /// OA_LATENCY_BUDGET_MS (default 220): max wait for uncached venue books per comparison.
    pub latency_budget: Duration,
    pub max_concurrent_compares: usize,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ConfigError {
    #[error("invalid {0}: {1}")]
    Invalid(&'static str, String),
    #[error(
        "OA_TRADING_MODE={0:?} is not a known mode (READ_ONLY, SIMULATION, TESTNET, MAINNET_NONCUSTODIAL)"
    )]
    UnknownMode(String),
    #[error(
        "OA_TRADING_MODE=TESTNET requires a binary built with `--features testnet` \
         (cargo build --release -p oa-app --features testnet)"
    )]
    TestnetNotBuilt,
    #[error(
        "TESTNET requires a separate database whose name contains \"testnet\" \
         (e.g. options_aggregator_testnet); refusing to mix testnet and mainnet records"
    )]
    TestnetNeedsSeparateDatabase,
    #[error(
        "OA_TRADING_MODE=MAINNET_NONCUSTODIAL requires a binary built with `--features mainnet-nc` \
         (cargo build --release -p oa-app --features mainnet-nc)"
    )]
    MainnetNcNotBuilt,
    #[error(
        "MAINNET_NONCUSTODIAL requires its own database whose name contains \"mainnet_nc\" \
         (e.g. options_aggregator_mainnet_nc); refusing to mix real-money and other records"
    )]
    MainnetNcNeedsSeparateDatabase,
    #[error(
        "OA_TRADING_MODE=LIVE is refused: custodial live execution is not implemented and requires \
         a dedicated, reviewed build plus explicit approval (user-signed mainnet execution is \
         MAINNET_NONCUSTODIAL)"
    )]
    LiveRefused,
    #[error("refusing to bind {0}: non-loopback addresses require OA_API_TOKEN to be set")]
    PublicBindWithoutToken(SocketAddr),
    #[error("OA_API_TOKEN must be at least 24 characters (openssl rand -hex 32)")]
    WeakApiToken,
    #[error("{0} still contains the CHANGE_ME placeholder from the example file")]
    Placeholder(&'static str),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Same as `from_env` with an injectable lookup (tests never touch the process env).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |k: &str| lookup(k).filter(|v| !v.trim().is_empty());
        let bind: SocketAddr = get("OA_BIND")
            .unwrap_or_else(|| "127.0.0.1:8080".into())
            .parse()
            .map_err(|e| ConfigError::Invalid("OA_BIND", format!("{e}")))?;
        let database_url = get("DATABASE_URL");
        let db_name = database_url
            .as_deref()
            .and_then(|u| u.rsplit('/').next())
            .map(|n| n.split('?').next().unwrap_or(n).to_string())
            .unwrap_or_default();
        let mode = match get("OA_TRADING_MODE").as_deref().unwrap_or("READ_ONLY") {
            "READ_ONLY" => TradingMode::ReadOnly,
            "SIMULATION" => TradingMode::Simulation,
            "TESTNET" => {
                if !cfg!(feature = "testnet") {
                    return Err(ConfigError::TestnetNotBuilt);
                }
                if !db_name.contains("testnet") {
                    return Err(ConfigError::TestnetNeedsSeparateDatabase);
                }
                TradingMode::Testnet
            }
            "MAINNET_NONCUSTODIAL" => {
                if !cfg!(feature = "mainnet-nc") {
                    return Err(ConfigError::MainnetNcNotBuilt);
                }
                if !db_name.contains("mainnet_nc") {
                    return Err(ConfigError::MainnetNcNeedsSeparateDatabase);
                }
                TradingMode::MainnetNonCustodial
            }
            "LIVE" => return Err(ConfigError::LiveRefused),
            other => return Err(ConfigError::UnknownMode(other.into())),
        };
        let api_token = get("OA_API_TOKEN");
        if api_token.as_ref().is_some_and(|t| t.trim().len() < 24) {
            return Err(ConfigError::WeakApiToken);
        }
        if database_url
            .as_ref()
            .is_some_and(|u| u.contains("CHANGE_ME"))
        {
            return Err(ConfigError::Placeholder("DATABASE_URL"));
        }
        if !bind.ip().is_loopback() && api_token.is_none() {
            return Err(ConfigError::PublicBindWithoutToken(bind));
        }
        Ok(Self {
            bind,
            database_url,
            mode,
            api_token,
            catalog_ttl: Duration::from_secs(300),
            default_book_max_age: Duration::from_secs(10),
            latency_budget: Duration::from_millis(
                get("OA_LATENCY_BUDGET_MS")
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(220)
                    .clamp(50, 15_000),
            ),
            // Comparisons are pure in-memory work once books are cached (~0.1-1 ms); the cap only
            // protects upstream venues on cold books. OA_MAX_CONCURRENT_COMPARES overrides.
            max_concurrent_compares: get("OA_MAX_CONCURRENT_COMPARES")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(512)
                .clamp(1, 100_000),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn defaults_are_read_only_on_loopback() {
        let c = cfg(&[]).unwrap();
        assert_eq!(c.mode, TradingMode::ReadOnly);
        assert!(c.bind.ip().is_loopback());
    }

    #[test]
    fn live_is_always_refused() {
        assert_eq!(
            cfg(&[("OA_TRADING_MODE", "LIVE")]).unwrap_err(),
            ConfigError::LiveRefused
        );
    }

    #[test]
    fn unknown_mode_refused() {
        assert!(matches!(
            cfg(&[("OA_TRADING_MODE", "live")]),
            Err(ConfigError::UnknownMode(_))
        ));
    }

    #[cfg(not(feature = "testnet"))]
    #[test]
    fn testnet_needs_the_testnet_build() {
        let r = cfg(&[
            ("OA_TRADING_MODE", "TESTNET"),
            ("DATABASE_URL", "postgres://x@h/options_aggregator_testnet"),
        ]);
        assert_eq!(r.unwrap_err(), ConfigError::TestnetNotBuilt);
    }

    #[cfg(feature = "testnet")]
    #[test]
    fn testnet_needs_a_separate_database() {
        let r = cfg(&[
            ("OA_TRADING_MODE", "TESTNET"),
            ("DATABASE_URL", "postgres://x@h/options_aggregator"),
        ]);
        assert_eq!(r.unwrap_err(), ConfigError::TestnetNeedsSeparateDatabase);
        let ok = cfg(&[
            ("OA_TRADING_MODE", "TESTNET"),
            ("DATABASE_URL", "postgres://x@h/options_aggregator_testnet"),
        ]);
        assert_eq!(ok.unwrap().mode, TradingMode::Testnet);
    }

    #[cfg(not(feature = "mainnet-nc"))]
    #[test]
    fn mainnet_nc_needs_the_mainnet_nc_build() {
        let r = cfg(&[
            ("OA_TRADING_MODE", "MAINNET_NONCUSTODIAL"),
            (
                "DATABASE_URL",
                "postgres://x@h/options_aggregator_mainnet_nc",
            ),
        ]);
        assert_eq!(r.unwrap_err(), ConfigError::MainnetNcNotBuilt);
    }

    #[cfg(feature = "mainnet-nc")]
    #[test]
    fn mainnet_nc_needs_its_own_database() {
        for db in ["options_aggregator", "options_aggregator_testnet"] {
            let url = format!("postgres://x@h/{db}");
            let r = cfg(&[
                ("OA_TRADING_MODE", "MAINNET_NONCUSTODIAL"),
                ("DATABASE_URL", &url),
            ]);
            assert_eq!(r.unwrap_err(), ConfigError::MainnetNcNeedsSeparateDatabase);
        }
        let ok = cfg(&[
            ("OA_TRADING_MODE", "MAINNET_NONCUSTODIAL"),
            (
                "DATABASE_URL",
                "postgres://x@h/options_aggregator_mainnet_nc",
            ),
        ]);
        assert_eq!(ok.unwrap().mode, TradingMode::MainnetNonCustodial);
    }

    #[test]
    fn public_bind_requires_token() {
        assert!(matches!(
            cfg(&[("OA_BIND", "0.0.0.0:8080")]),
            Err(ConfigError::PublicBindWithoutToken(_))
        ));
        let token = ("OA_API_TOKEN", "0123456789abcdef0123456789abcdef"); // gitleaks:allow
        assert!(cfg(&[("OA_BIND", "0.0.0.0:8080"), token]).is_ok());
        assert!(matches!(
            cfg(&[("OA_BIND", "0.0.0.0:8080"), ("OA_API_TOKEN", "t")]),
            Err(ConfigError::WeakApiToken)
        ));
        assert!(matches!(
            cfg(&[("DATABASE_URL", "postgres://oa:CHANGE_ME@db/x")]),
            Err(ConfigError::Placeholder(_))
        ));
    }
}
