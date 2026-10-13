# MCP server

`oa-mcp` is a stdio [Model Context Protocol](https://modelcontextprotocol.io) server (JSON-RPC 2.0,
newline-delimited). It is a thin facade over the HTTP API: each tool maps to one endpoint, so an
agent goes through exactly the same engine, validation, comparability rules and limits as the API
and the web UI.

| Tool | Endpoint |
|---|---|
| `list_platforms` | `GET /v1/platforms` |
| `search_instruments` | `GET /v1/instruments` |
| `compare_quotes` | `POST /v1/quotes/compare` |
| `explain_comparability` | `POST /v1/comparability/explain` |
| `preview_route` | `POST /v1/routes/preview` |
| `simulate_order` | `POST /v1/orders/simulate` |
| `quote_route` | `POST /v1/routes/quote` |
| `execute_testnet_route` | `POST /v1/routes/execute-testnet` — refused unless the MCP process has `OA_MCP_ALLOW_TESTNET_EXECUTION=true` |
| `get_order_status` | `GET /v1/orders/{id}` |
| `get_exec_stats` | `GET /v1/exec-stats` |

There is no mainnet execution tool and no tool can change the trading mode. Tool results are
prefixed with a notice that venue-originated text is untrusted data. `accepted_differences` must
come from the user; the tool description says so. Arguments are validated against the API's Rust
types before being forwarded.

Protocol versions: 2025-06-18, 2025-03-26, 2024-11-05.

## Run

Start the API, build the binary (`cargo build --release -p oa-app --bin oa-mcp`), then let the MCP
client launch it. Example configuration ([`mcp.example.json`](../mcp.example.json)):

```json
{
  "mcpServers": {
    "omni-strike": {
      "command": "/absolute/path/to/omni-strike/target/release/oa-mcp",
      "env": { "OA_API_URL": "http://127.0.0.1:8080" }
    }
  }
}
```

Claude Code: `claude mcp add omni-strike --env OA_API_URL=http://127.0.0.1:8080 -- /absolute/path/to/oa-mcp`

If the server requires a token, set `OA_API_TOKEN` in the MCP `env` as well.
