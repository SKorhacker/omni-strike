//! MCP stdio server. Forwards to the local API (OA_API_URL, default http://127.0.0.1:8080).
//! stdout carries protocol messages only; diagnostics go to stderr.

use oa_app::mcp::{ApiClient, handle};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let base = std::env::var("OA_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let token = std::env::var("OA_API_TOKEN").ok().filter(|t| !t.is_empty());
    let mut api = ApiClient::new(&base, token);
    api.allow_testnet_execution = std::env::var("OA_MCP_ALLOW_TESTNET_EXECUTION")
        .map(|v| v == "true")
        .unwrap_or(false);
    eprintln!(
        "oa-mcp: forwarding to {base} (testnet execution via MCP: {})",
        api.allow_testnet_execution
    );

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg, &api).await,
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null,
                                  "error": {"code": -32700, "message": format!("parse error: {e}")}})),
        };
        if let Some(r) = reply {
            let mut s = r.to_string();
            s.push('\n');
            if out.write_all(s.as_bytes()).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    }
}
