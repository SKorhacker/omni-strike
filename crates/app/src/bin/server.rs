//! HTTP server. Requires DATABASE_URL (PostgreSQL).
//! Modes: READ_ONLY / SIMULATION; TESTNET only in a `--features testnet` build (testnet market
//! data, separate database); MAINNET_NONCUSTODIAL only in a `--features mainnet-nc` build (mainnet
//! data, separate database, user-signed execution only); custodial LIVE always refused (config.rs).

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use oa_adapters::Connector;
use oa_adapters::aevo::AevoConnector;
use oa_adapters::deribit::DeribitConnector;
use oa_adapters::derive::DeriveConnector;
use oa_adapters::fx::UsdtAsUsdc;
use oa_adapters::paradex::ParadexConnector;
use oa_adapters::thetanuts::ThetanutsConnector;
use oa_app::api::{AppState, router};
use oa_app::config::Config;
use oa_app::engine::Engine;
use oa_app::metrics::Metrics;
use oa_app::store::{PgStore, Store};
use oa_core::domain::TradingMode;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `.env` by default; a testnet profile uses its own file (e.g. OA_ENV_FILE=.env.testnet).
    let env_file = std::env::var("OA_ENV_FILE").unwrap_or_else(|_| ".env".into());
    let _ = dotenvy::from_filename(&env_file);
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // OA_LOG_FORMAT=json: one JSON object per line (log shippers, alongside Caddy's JSON logs).
    if std::env::var("OA_LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json")) {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    let cfg = Config::from_env()?;
    let profile = match cfg.mode {
        TradingMode::Testnet => "testnet",
        TradingMode::MainnetNonCustodial => "mainnet-nc",
        _ => "mainnet",
    };
    let _span = tracing::info_span!("oa", profile).entered();
    let url = cfg
        .database_url
        .clone()
        .ok_or("DATABASE_URL is not set (see README: Run locally)")?;
    // Never log the URL: it contains the password.
    let store = Arc::new(
        PgStore::connect(&url)
            .await
            .map_err(|e| format!("database: {e}"))?,
    );
    tracing::info!(env_file, "database connected, migrations applied");
    let _ = store
        .audit(
            "startup",
            Some(profile),
            &serde_json::json!({"mode": cfg.mode, "profile": profile, "env_file": env_file}),
        )
        .await;

    // Persistent Derive WebSocket subscription (one-shot snapshot only as fallback).
    let connectors: Vec<Arc<dyn Connector>> = match cfg.mode {
        TradingMode::Testnet => vec![
            // v3 testnet: mintable test tokens + the execution venue below share this network.
            Arc::new(DeriveConnector::testnet_v3().with_stream()),
            Arc::new(AevoConnector::testnet()),
            Arc::new(ParadexConnector::testnet()),
            // CEX test environments (prices from the SAME environment the orders go to).
            Arc::new(UsdtAsUsdc::new(oa_adapters::network::OnNetwork::new(
                oa_adapters::bybit::BybitConnector::new(oa_adapters::bybit::TESTNET_REST)
                    .with_stream(),
                oa_adapters::bybit_exec::TESTNET_NETWORK,
            ))),
            Arc::new(oa_adapters::network::OnNetwork::new(
                DeribitConnector::new(oa_adapters::deribit::TESTNET_REST).with_stream(),
                oa_adapters::deribit_exec::TESTNET_NETWORK,
            )),
            Arc::new(oa_adapters::network::OnNetwork::new(
                oa_adapters::okx::OkxConnector::demo().with_stream(),
                oa_adapters::okx_exec::DEMO_NETWORK,
            )),
        ],
        _ => vec![
            Arc::new(DeriveConnector::default().with_stream()),
            Arc::new(AevoConnector::default()),
            Arc::new(ParadexConnector::default()),
            Arc::new(ThetanutsConnector::default()),
            // CallPut (Base): pool-priced crypto + synthetic stock/ETF options.
            Arc::new(oa_adapters::callput::CallputConnector::default()),
            // Hypercall (Hyperliquid): crypto + index/equity options; books mostly empty (RFQ).
            Arc::new(oa_adapters::hypercall::HypercallConnector::default()),
            // Hegic (Arbitrum): on-chain premiums for listed expiries/strikes (ask only).
            Arc::new(oa_adapters::hegic::HegicConnector::default()),
            // Rysk: indicative sell-side levels in USDT (shown, never routed).
            Arc::new(UsdtAsUsdc::new(oa_adapters::rysk::RyskConnector::default())),
            Arc::new(DeribitConnector::default().with_stream()),
            Arc::new(oa_adapters::okx::OkxConnector::default().with_stream()),
            Arc::new(oa_adapters::bullish::BullishConnector::default()),
            Arc::new(oa_adapters::coincall::CoincallConnector::default()),
            // USDT-quoted venues: converted to USDC at the live Kraken rate (fx.rs).
            Arc::new(UsdtAsUsdc::new(
                oa_adapters::bybit::BybitConnector::default().with_stream(),
            )),
            Arc::new(UsdtAsUsdc::new(
                oa_adapters::binance::BinanceConnector::default().with_stream(),
            )),
            Arc::new(UsdtAsUsdc::new(oa_adapters::gate::GateConnector::default())),
            Arc::new(UsdtAsUsdc::new(
                oa_adapters::delta::DeltaConnector::default(),
            )),
            // Prediction-market binaries (family = binary).
            Arc::new(oa_adapters::kalshi::KalshiConnector::default()),
            Arc::new(oa_adapters::hyperliquid::HyperliquidConnector::default()),
            Arc::new(oa_adapters::myriad::MyriadConnector::default()),
            Arc::new(oa_adapters::limitless::LimitlessConnector::default()),
        ],
    };
    let engine = Arc::new(
        Engine::new(
            connectors,
            store.clone(),
            Arc::new(Metrics::default()),
            cfg.mode,
            cfg.catalog_ttl,
            cfg.default_book_max_age,
            cfg.max_concurrent_compares,
        )
        .with_latency_budget(cfg.latency_budget),
    );
    if cfg.mode == TradingMode::Testnet {
        attach_testnet_execution(&engine, store.clone()).await;
    }
    if cfg.mode == TradingMode::MainnetNonCustodial {
        attach_mainnet_noncustodial_execution(&engine, store.clone()).await;
    }
    {
        let e = engine.clone();
        tokio::spawn(async move { e.warm_up().await });
        engine.spawn_catalog_refresher();
        engine.spawn_stream_prewarm();
    }

    spawn_retention(store.clone());
    let integrators =
        std::sync::Arc::new(oa_app::integrators::Integrators::load(store.clone()).await?);
    tracing::info!(count = integrators.list().len(), "integrators loaded");
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, mode = ?cfg.mode, profile, auth = cfg.api_token.is_some(), "listening");
    let stop = Arc::new(tokio::sync::Notify::new());
    let stop_wait = stop.clone();
    let server = axum::serve(
        listener,
        router({
            let mut st = AppState::new(engine.clone(), cfg.api_token);
            st.integrators = integrators.clone();
            st.admin_token = std::env::var("OA_ADMIN_TOKEN")
                .ok()
                .filter(|t| t.trim().len() >= 24);
            st.require_api_key = std::env::var("OA_REQUIRE_API_KEY")
                .is_ok_and(|v| v.trim().eq_ignore_ascii_case("true"));
            st.public_limiter = Arc::new(oa_app::api::IpLimiter::from_env());
            // Live "router vs Derive" dashboard feed (OA_LIVE=off to disable).
            if std::env::var("OA_LIVE").map(|v| v != "off").unwrap_or(true) {
                st.live = Some(oa_app::live::LiveTracker::spawn(&engine));
            }
            st
        })
        .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        tracing::info!("shutdown requested: draining (25 s max)");
        stop.notify_waiters();
    });
    // Graceful drain is bounded: long-lived SSE streams would otherwise keep it waiting forever.
    tokio::select! {
        r = server => r?,
        _ = async {
            stop_wait.notified().await;
            tokio::time::sleep(std::time::Duration::from_secs(25)).await;
        } => tracing::warn!("drain deadline reached: closing remaining connections"),
    }
    // Stops the venue actors and flushes queued observations.
    engine.shutdown().await;
    tracing::info!("stopped cleanly");
    Ok(())
}

/// Hourly purge of expired single-use rows (route quotes, prepared executions: after 1 day) and of
/// simulations older than OA_SIMULATION_RETENTION_DAYS (default 30). Keeps a public deployment's
/// database bounded; the order journal and audit log are kept.
fn spawn_retention(store: Arc<PgStore>) {
    let days: i64 = std::env::var("OA_SIMULATION_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|d| *d >= 1)
        .unwrap_or(30);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let now = chrono::Utc::now();
            match store
                .purge(
                    now - chrono::Duration::days(1),
                    now - chrono::Duration::days(days),
                )
                .await
            {
                Ok(n) if n > 0 => tracing::info!(rows = n, "retention purge"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "retention purge failed"),
            }
        }
    });
}

/// SIGINT (Ctrl+C) or SIGTERM (Docker, systemd, Kubernetes).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match term.as_mut() {
                    Some(t) => { t.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// TESTNET only: Aevo testnet execution from OA_AEVO_TESTNET_* (never logged). Missing keys keep
/// the server running read-only on testnet data, with the reason logged (names only).
async fn attach_testnet_execution(engine: &Arc<Engine>, store: Arc<PgStore>) {
    use oa_adapters::aevo_exec::AevoTestnetExec;
    use oa_adapters::derive_exec::DeriveTestnetExec;
    use oa_adapters::exec::ExecutionVenue;
    use oa_app::execution::ExecutionService;
    let kill_switch = std::env::var("OA_KILL_SWITCH")
        .map(|v| v.trim() != "false")
        .unwrap_or(true);
    let mut venues: Vec<Arc<dyn ExecutionVenue>> = vec![];
    // Each venue is optional and independent; missing variables are logged by NAME only.
    match AevoTestnetExec::testnet_from_env() {
        Ok(v) => {
            tracing::warn!(target: "testnet", identity = %v.identity(), "Aevo testnet execution enabled");
            venues.push(Arc::new(v));
        }
        Err(e) => {
            tracing::warn!(target: "testnet", error = %e, "Aevo testnet execution NOT available")
        }
    }
    macro_rules! attach {
        ($name:literal, $ctor:expr) => {
            match $ctor {
                Ok(v) => {
                    tracing::warn!(target: "testnet", identity = %v.identity(), concat!($name, " testnet execution enabled"));
                    venues.push(Arc::new(v));
                }
                Err(e) => tracing::warn!(target: "testnet", error = %e, concat!($name, " testnet execution NOT available")),
            }
        };
    }
    attach!(
        "Bybit",
        oa_adapters::bybit_exec::BybitTestnetExec::testnet_from_env()
    );
    attach!(
        "Deribit",
        oa_adapters::deribit_exec::DeribitTestnetExec::testnet_from_env()
    );
    attach!(
        "OKX demo",
        oa_adapters::okx_exec::OkxDemoExec::testnet_from_env()
    );
    let noncustodial = std::env::var("OA_DERIVE_NONCUSTODIAL")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if std::env::var("OA_PARADEX_NONCUSTODIAL").is_ok_and(|v| v.eq_ignore_ascii_case("true")) {
        // The user's Starknet account signs; its short-lived JWT is used once and never stored.
        let v = oa_adapters::paradex_user::ParadexUserSignedExec::testnet();
        tracing::warn!(target: "testnet", identity = %v.identity(), "Paradex testnet NON-CUSTODIAL execution enabled");
        venues.push(Arc::new(v));
    }
    if std::env::var("OA_AEVO_NONCUSTODIAL").is_ok_and(|v| v.eq_ignore_ascii_case("true")) {
        // The END USER signs and authenticates every Aevo order; this server holds no Aevo secret.
        let v = oa_adapters::aevo_user::AevoUserSignedExec::testnet();
        tracing::warn!(target: "testnet", identity = %v.identity(), "Aevo testnet NON-CUSTODIAL execution enabled");
        venues.retain(|x| x.platform() != oa_core::domain::PlatformId::Aevo);
        venues.push(Arc::new(v));
    }
    if noncustodial {
        // The END USER signs every order; this server holds no Derive key.
        let v = oa_adapters::derive_user::DeriveUserSignedExec::testnet();
        tracing::warn!(target: "testnet", identity = %v.identity(), "Derive v3 testnet NON-CUSTODIAL execution enabled");
        venues.push(Arc::new(v));
    } else {
        match DeriveTestnetExec::testnet_from_env() {
            Ok(v) => {
                tracing::warn!(target: "testnet", identity = %v.identity(), "Derive v3 testnet execution enabled");
                venues.push(Arc::new(v));
            }
            Err(e) => {
                tracing::warn!(target: "testnet", error = %e, "Derive testnet execution NOT available")
            }
        }
    }
    if venues.is_empty() {
        tracing::warn!(target: "testnet", "TESTNET execution NOT available (read-only testnet data only)");
        return;
    }
    let exec = ExecutionService::new(TradingMode::Testnet, venues, store, kill_switch);
    exec.start_tracker();
    match engine.attach_execution(exec) {
        Ok(()) => {
            tracing::warn!(target: "testnet", kill_switch, "TESTNET execution service attached")
        }
        Err(e) => tracing::error!(target: "testnet", error = %e, "could not attach execution"),
    }
}

/// MAINNET NON-CUSTODIAL: only user-signed venues, each opted in with OA_<VENUE>_NONCUSTODIAL=true.
/// This function deliberately has no access to any key loader: nothing here can sign for a user.
/// Kill switch ON unless OA_KILL_SWITCH=false; caps from OA_MAINNET_MAX_ORDER_USD / _SESSION_USD.
async fn attach_mainnet_noncustodial_execution(engine: &Arc<Engine>, store: Arc<PgStore>) {
    use oa_adapters::exec::ExecutionVenue;
    use oa_app::execution::ExecutionService;
    let kill_switch = std::env::var("OA_KILL_SWITCH")
        .map(|v| v.trim() != "false")
        .unwrap_or(true);
    let on = |k: &str| std::env::var(k).is_ok_and(|v| v.trim().eq_ignore_ascii_case("true"));
    let mut venues: Vec<Arc<dyn ExecutionVenue>> = vec![];
    if on("OA_DERIVE_NONCUSTODIAL") {
        venues.push(Arc::new(
            oa_adapters::derive_user::DeriveUserSignedExec::mainnet(),
        ));
    }
    if on("OA_AEVO_NONCUSTODIAL") {
        venues.push(Arc::new(
            oa_adapters::aevo_user::AevoUserSignedExec::mainnet(),
        ));
    }
    if on("OA_PARADEX_NONCUSTODIAL") {
        venues.push(Arc::new(
            oa_adapters::paradex_user::ParadexUserSignedExec::mainnet(),
        ));
    }
    if on("OA_THETANUTS_NONCUSTODIAL") {
        // The user's wallet sends fillOrder on Base; we only read the public receipt.
        let referrer = std::env::var("OA_THETANUTS_REFERRER")
            .ok()
            .and_then(|a| oa_adapters::aevo_exec::parse_address(a.trim()));
        venues.push(Arc::new(
            oa_adapters::thetanuts_user::ThetanutsUserTxExec::mainnet(referrer),
        ));
    }
    if on("OA_CALLPUT_NONCUSTODIAL") {
        venues.push(Arc::new(
            oa_adapters::callput_user::CallputUserTxExec::mainnet(),
        ));
    }
    if on("OA_HYPERCALL_NONCUSTODIAL") {
        venues.push(Arc::new(
            oa_adapters::hypercall_user::HypercallUserSignedExec::production(),
        ));
    }
    if on("OA_HYPERLIQUID_NONCUSTODIAL") {
        venues.push(Arc::new(
            oa_adapters::hyperliquid_user::HyperliquidUserSignedExec::mainnet(),
        ));
    }
    if on("OA_LIMITLESS_NONCUSTODIAL") {
        venues.push(Arc::new(
            oa_adapters::limitless_user::LimitlessUserSignedExec::production(),
        ));
    }
    for v in &venues {
        tracing::warn!(target: "mainnet-nc", identity = %v.identity(), "MAINNET non-custodial venue enabled");
    }
    if venues.is_empty() {
        tracing::warn!(target: "mainnet-nc", "no OA_<VENUE>_NONCUSTODIAL=true: read-only mainnet data");
        return;
    }
    let exec = ExecutionService::new(TradingMode::MainnetNonCustodial, venues, store, kill_switch);
    exec.start_tracker();
    let limits = oa_app::engine::risk_limits(TradingMode::MainnetNonCustodial);
    match engine.attach_execution(exec) {
        Ok(()) => {
            tracing::warn!(target: "mainnet-nc", kill_switch, max_order = %limits.max_order_cost,
            max_session = %limits.max_session_cost, "MAINNET NON-CUSTODIAL execution attached")
        }
        Err(e) => tracing::error!(target: "mainnet-nc", error = %e, "could not attach execution"),
    }
}
