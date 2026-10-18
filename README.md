# tx-watch-core

> Real-time Soroban smart contract monitoring and webhook alert engine for the Stellar network.

Part of the [TxWatch](https://github.com/Tx-wats) ecosystem.

---

## What is this?

**TxWatch** sits between the [Stellar Horizon REST API](https://developers.stellar.org/docs/data/apis/horizon)
and your infrastructure. It polls every contract you configure, evaluates alert rules against
each new transaction, and fires a JSON webhook the moment a condition is met — no SDK, no
subscriptions, no infrastructure beyond a single Rust binary.

```
  Stellar Network
       │
       ▼
  Horizon REST API          ← TxWatch polls this
  (testnet / mainnet /
   futurenet)
       │
       ▼
  txwatch-poller            ← fetches /accounts/{contract}/transactions
       │                       fetches /transactions/{hash}/operations
       ▼
  txwatch-rules             ← evaluates AlertRules against each transaction
       │
       ▼
  txwatch-notifier          ← POSTs AlertPayload JSON to your webhook URL
       │
       ▼
  Your webhook receiver     ← Slack, PagerDuty, custom API, etc.
```

---

## Stellar / Soroban primer

| Concept | What it means here |
|---|---|
| **Stellar** | Layer-1 blockchain with fast finality (~5 s) and low fees |
| **Soroban** | Stellar's smart contract platform (WebAssembly-based) |
| **Horizon** | The REST API gateway to the Stellar network — TxWatch's data source |
| **Contract address** | A 56-character string starting with `C` (e.g. `CABC...`) |
| **XLM** | Stellar's native asset; 1 XLM = 10,000,000 stroops |
| **Stroop** | Smallest unit of XLM (like satoshi for Bitcoin) |
| **Paging token** | Horizon cursor used to fetch only new transactions since last poll |
| **invoke_host_function** | The Horizon operation type for a Soroban contract call |

### Horizon endpoints used

| Endpoint | Purpose |
|---|---|
| `GET /accounts/{contract_id}/transactions?cursor=…&order=asc` | Fetch new transactions for a contract |
| `GET /transactions/{hash}/operations` | Fetch operations to extract function name and payment amount |

### Network base URLs

| Network | Horizon base URL |
|---|---|
| Mainnet | `https://horizon.stellar.org` |
| Testnet | `https://horizon-testnet.stellar.org` |
| Futurenet | `https://horizon-futurenet.stellar.org` |
| Custom / local | Your own `horizon_url`, e.g. `http://localhost:8000` for `stellar/quickstart --local` |

---

## Quickstart

```bash
# 1. Clone
git clone https://github.com/Tx-wats/core
cd core

# 2. Copy and edit the example config (./txwatch.toml is the default path)
cp config/example.toml txwatch.toml
$EDITOR txwatch.toml

# 3. Validate your config
cargo run -p txwatch -- validate

# 4. Send a test webhook to confirm your receiver works
cargo run -p txwatch -- test-webhook --url https://hooks.example.com/my-webhook

# 5. Start watching
cargo run -p txwatch -- watch
```

To pick up config changes without restarting (and without losing cursors),
send `SIGHUP`: `kill -HUP <pid>`. An invalid file is logged and ignored.

Set `RUST_LOG=debug` for verbose output. This also enables per-contract idle
poll logs — the poller emits `"no new transactions"` debug messages with the
contract label and current cursor when a poll finds nothing new.

---

## Docker

Build the image:

```bash
docker build -t txwatch .
```

Run with a config file (the image reads `TXWATCH_CONFIG=/config/txwatch.toml`):

```bash
docker run -v $(pwd)/txwatch.toml:/config/txwatch.toml txwatch watch
```

Or validate your config:

```bash
docker run -v $(pwd)/txwatch.toml:/config/txwatch.toml txwatch validate
```

To watch contracts on a local standalone network, `docker compose -f docker-compose.local.yml up`
starts `stellar/quickstart --local` together with TxWatch using [`config/local.toml`](config/local.toml).

For Kubernetes, a Helm chart lives in [`deploy/helm/txwatch`](deploy/helm/txwatch); see [docs/kubernetes.md](docs/kubernetes.md).

For local development with webhook testing, see [Local Development with Docker Compose](#local-development-with-docker-compose) in [CONTRIBUTING.md](CONTRIBUTING.md).

---

## CLI

```
txwatch [--config <path>] [--horizon-url <URL>] <command>

Commands:
  watch [--once] [--dry-run]     Start the polling engine
  validate [--format text|json]  Validate the config file and print a summary
  test-webhook --url <URL>       Send a test payload to a webhook URL and exit
  replay --contract <label> --tx <hash> [--send]
                                 Evaluate a contract's rules against one transaction
  init                           Write a starter config (prompts for anything not passed)
  completions <shell>            Print a completion script (bash, zsh, fish, powershell, elvish)
  man                            Print the txwatch(1) man page
```

Install completions with e.g. `txwatch completions bash > ~/.local/share/bash-completion/completions/txwatch`
or `txwatch completions zsh > "${fpath[1]}/_txwatch"`, and the man page with
`txwatch man > ~/.local/share/man/man1/txwatch.1`. Release archives also ship a
`completions-and-man` bundle with both pre-generated.

Start a new config with `txwatch init --contract-id C... --network testnet --webhook-url https://...`
(add `--output <path>`, default `txwatch.toml`). Any value not passed as a flag is prompted for.
The generated file is validated before it's written, and an existing file is only replaced with
`--force`.

`replay` fetches a historical transaction and its operations from Horizon, runs the named
contract's rules against it and prints every matched rule with its webhook payload, so rule authors
can check "would my rules have fired for transaction X?". Nothing is sent unless `--send` is given.

`validate --format json` prints the parsed config as a single JSON object (webhook secrets are
redacted to `webhook_secret_set`), or `{"valid": false, "error": "..."}` with exit code 1.

`--horizon-url <URL>` overrides the Horizon base URL
for every contract (for example a private Horizon instance).

`watch --once` runs a single poll cycle, delivers any alerts, saves cursors to `cursor_file` (if
configured) and exits, for use from cron, CI jobs or serverless schedulers. It exits `1` if any
contract poll or webhook delivery failed, `0` otherwise. Set `cursor_file` so each run picks up
where the previous one stopped; without it every run starts from `now`.
The config path comes from `--config`, else the `TXWATCH_CONFIG` environment variable, else
`./txwatch.toml`. TxWatch exits with an error if that file does not exist.

---

## Config

See [docs/configuration.md](docs/configuration.md) for the full reference.

```toml
poll_interval_seconds = 10

[[contracts]]
label       = "My Escrow Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/my-webhook"

  [[contracts.rules]]
  type          = "LargeTransfer"
  threshold_xlm = 10000

  [[contracts.rules]]
  type           = "AdminFunctionCalled"
  function_names = ["set_admin", "upgrade", "initialize"]
  webhook_url    = "https://hooks.example.com/critical-webhook"
  severity       = "critical"

  [[contracts.rules]]
  type    = "TransactionFailed"
  enabled = false

  [[contracts.rules]]
  type  = "SourceAccount"
  deny  = ["GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN"]

  [[contracts.rules]]
  type = "All"
  [[contracts.rules.rules]]
  type          = "FunctionCalled"
  function_name = "withdraw"
  [[contracts.rules.rules]]
  type          = "LargeTransfer"
  threshold_xlm = 50000
```

---

## Alert rules

| Rule | Triggers when… |
|---|---|
| `AnyTransaction` | Any transaction touches the contract |
| `TransactionFailed` | A transaction fails (`successful = false`) |
| `LargeTransfer` | Payment amount ≥ `threshold_xlm` XLM |
| `FunctionCalled` | A specific Soroban function is invoked |
| `AdminFunctionCalled` | Any function in a named list is invoked |
| `HighFee` | Transaction fee is greater than or equal to `threshold_stroops` (or `threshold_xlm`) |
| `EventEmitted` | The transaction emitted a contract event whose first topic is a given symbol |
| `SourceAccount` | Transaction source account matches an allow/deny list |
| `All` | All nested rules match (logical AND) |
| `Any` | Any nested rule matches (logical OR) |
| `Not` | Nested rule does not match (logical NOT) |

Every rule entry also supports:
- `enabled = false` — silence a rule without removing it; shown as `(disabled)` in `txwatch validate`
- `webhook_url` — per-rule webhook URL override (falls back to the contract's URL)
- `webhook_secret` — per-rule webhook secret override
- `severity` — `info`, `warning`, or `critical`; included in the alert payload

Any rule can set `cooldown_seconds` to send at most one alert per window for that rule on that contract;
suppressed matches are reported in `suppressed_count` on the next alert.

See [docs/alert-rules.md](docs/alert-rules.md) for full details.

---

## Webhook payload

```json
{
  "schema_version":   1,
  "alert_id":         "3f2b9c1d8e7a6b5c4d3e2f1a0b9c8d7e",
  "label":            "My Escrow Contract",
  "contract_id":      "CAAA...",
  "network":          "testnet",
  "rule_type":        "LargeTransfer",
  "rule_triggered":   "LargeTransfer(>=10000XLM)",
  "transaction_hash": "abc123...",
  "function_name":    "transfer",
  "function_names":   ["transfer"],
  "amount_xlm":       15000,
  "amount_stroops":   150000000000000,
  "amount_xlm_decimal": "15000.0000000",
  "fee_charged_stroops": 50000,
  "source_account":   "GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN",
  "severity":         "critical",
  "timestamp":        1705316096,
  "timestamp_iso":    "2024-01-15T12:00:00Z",
  "horizon_link":     "https://horizon-testnet.stellar.org/transactions/abc123...",
  "explorer_link":    "https://stellar.expert/explorer/testnet/tx/abc123...",
  "resolved":         false,
  "matched_events":   [],
  "suppressed_count": 0
}
```

**Webhook headers:**
- `Content-Type: application/json`
- `Content-Length: <length of JSON body in bytes>`
- `X-TxWatch-Version: <package version>`
- `X-TxWatch-Alert-Id: <alert_id>` (same value as the `alert_id` body field — usable for deduplication without parsing the body)
- `X-TxWatch-Signature: sha256=<hmac>` (optional, only when `webhook_secret` is configured — HMAC-SHA256 of the request body; see [Verifying webhook signatures](docs/verifying-webhooks.md))
- `X-TxWatch-Secret: <webhook_secret>` (optional, only when `webhook_secret` is configured — the raw secret; verify the signature instead where possible)
- Any custom headers from `webhook_headers` (e.g. `Authorization: Bearer ${TOKEN}`)

**Destinations and formats:** a contract can deliver to several receivers at once via
`[[contracts.webhooks]]`, and each destination can use `format = "slack"`, `"discord"` or
`"pagerduty"` instead of the JSON above, so Slack, Discord and PagerDuty work without an adapter
service. See [Multiple destinations](docs/configuration.md#multiple-destinations) and
[Webhook formats](docs/configuration.md#webhook-formats).

**Fields:**
- `schema_version` — integer version of this payload shape (currently `1`). Additive changes (new optional fields) keep the same version; breaking changes (field removals or renames) bump it. Receivers should use this to detect incompatible changes.
- `alert_id` — stable, deterministic identifier derived from `(network, contract_id, tx_hash, rule_type, rule_triggered)` via SHA-256 prefix (32 hex chars). Identical for every retry of the same alert. Receivers should deduplicate on this value.
- `rule_type` — stable machine-readable rule variant (e.g. `"LargeTransfer"`, `"HighFee"`); use this for programmatic routing
- `rule_triggered` — human-readable rule description with parameters (e.g. `"LargeTransfer(>=10000XLM)"`); use this for display
- `function_name` — the first invoked Soroban function name, or `null` for non-Soroban transactions.
- `function_names` — all invoked Soroban function names in the transaction (may contain multiple entries for multi-op transactions).
- `source_account` — the G-address that submitted the transaction; omitted from the payload when not present on the Horizon record.
- `severity` — the severity level set on the matching rule (`"info"`, `"warning"`, or `"critical"`); omitted when not configured.
- `amount_xlm` — transfer amount in whole XLM (truncated integer, e.g. `9999` for a 9,999.99 XLM transfer), or `null`. Kept for backward compatibility — use `amount_xlm_decimal` for precise accounting.
- `amount_stroops` — raw transfer amount in stroops (1 XLM = 10,000,000 stroops), or `null`.
- `amount_xlm_decimal` — transfer amount as a decimal string with 7 fractional digits (e.g. `"9999.9900000"`), or `null`. Use this instead of `amount_xlm` when precision matters.
- `matched_events` — for `EventEmitted` alerts, the matching contract events (`contract_id`, `topics`, `data`, as decoded `ScVal` JSON); empty for other rules.
- `suppressed_count` — matches of this rule suppressed by its `cooldown_seconds` since the previous alert; `0` otherwise.
- `horizon_link` — direct Horizon REST API URL for the transaction (e.g. `https://horizon-testnet.stellar.org/transactions/<hash>`); useful for fetching raw XDR or operation details programmatically.
- `explorer_link` — Stellar Expert web explorer URL for the transaction (e.g. `https://stellar.expert/explorer/testnet/tx/<hash>`); useful for human-readable inspection in a browser.

> **`horizon_link` vs `explorer_link`**: `horizon_link` points to the Horizon REST API endpoint and returns raw JSON — useful for programmatic access. `explorer_link` points to the [Stellar Expert](https://stellar.expert) web UI — useful for human inspection. Both are always present for every alert regardless of rule type.

---

## Architecture

```
txwatch (cli binary)
  │
  ├── txwatch-config      TOML parsing · contract ID validation · rule validation
  │
  └── txwatch-poller      Horizon polling loop · cursor tracking · op enrichment
        │
        ├── txwatch-rules     AlertRule evaluation · AlertPayload construction
        │
        └── txwatch-notifier  Webhook POST · 3-attempt exponential backoff · tracing logs
```

### Crate responsibilities

| Crate | Responsibility |
|---|---|
| `txwatch-config` | Parse `config.toml` into typed structs; validate all fields |
| `txwatch-rules` | Pure rule evaluation — no I/O, fully unit-testable |
| `txwatch-notifier` | HTTP webhook delivery with retry; timestamped structured logs |
| `txwatch-poller` | Horizon REST client; cursor map; per-transaction error isolation |
| `txwatch` (cli) | `clap` binary; tracing init; subcommand dispatch |

---

## Tracing and observability

TxWatch uses `tracing` spans to correlate work across each poll cycle and webhook delivery.
- `txwatch-poller::poll_contract` is instrumented with `contract`, `contract_id`, and `network` fields.
- `txwatch-poller::fetch_soroban_details` is instrumented with the transaction hash.
- `txwatch-notifier::send_webhook` creates a span with `contract` and `rule` fields before sending the webhook request.

Set `RUST_LOG=info` or a more specific filter to view structured tracing output in the CLI.

Logs are human-readable text by default. For log pipelines (Loki, Datadog, CloudWatch), pass
`--log-format json` or set `TXWATCH_LOG_FORMAT=json` to emit one JSON object per line. Event
fields (`contract`, `tx`, `rule`, `attempt`, ...) stay as JSON fields, and each line includes the
current span and the full span list:

```sh
txwatch --log-format json watch
TXWATCH_LOG_FORMAT=json txwatch watch
```

### Prometheus metrics (optional)

Build the `txwatch` binary with the `metrics` feature and pass `--metrics-addr` to `watch`:

```bash
cargo build --release -p txwatch --features metrics
./target/release/txwatch --config config.toml watch --metrics-addr 127.0.0.1:9090
```

This serves `GET /metrics` (Prometheus text format), `GET /healthz` (process alive) and
`GET /readyz` (200 once a poll has succeeded recently, else 503). Exported series include
`txwatch_transactions_total`, `txwatch_alerts_total` and `txwatch_webhook_failures_total`
(labelled by `contract` and `network`), `txwatch_alerts_suppressed_total`
(`contract`, `network`, `rule_type`; matches dropped by a cooldown),
`txwatch_webhook_deliveries_total` (`contract`, `format`, `destination`, `outcome`; `destination`
is the destination's index, never its URL), the Horizon request latency histogram, the webhook
delivery latency histogram `txwatch_webhook_delivery_duration_seconds` (`format`, `outcome`;
buckets up to 60 s), per-contract freshness gauges, `txwatch_ledger_lag` (ledgers behind
Horizon's latest ledger; a warning is logged above `TXWATCH_LEDGER_LAG_WARN_THRESHOLD`,
default 720) and `txwatch_build_info`. Series of contracts removed or relabelled by a reload
are deleted. Without the feature (the default build), the `--metrics-addr` flag doesn't exist.

Library users can call `txwatch_poller::serve_metrics(addr, shutdown)` themselves before
`run_with_shutdown`, with `txwatch-poller`'s `metrics` feature enabled.

The endpoint can be scraped by any Prometheus-compatible monitoring stack (Prometheus, Grafana
Agent, VictoriaMetrics, etc.).

Example scrape config:

```yaml
scrape_configs:
  - job_name: txwatch
    static_configs:
      - targets: ['localhost:9090']
```

Ready-made alerting rules for TxWatch's own health (stale polls, consecutive poll failures,
webhook failure rate) are in [`deploy/prometheus/txwatch-alerts.yml`](deploy/prometheus/txwatch-alerts.yml)
and a Grafana dashboard is in [`deploy/grafana/txwatch.json`](deploy/grafana/txwatch.json).

---

## How a transaction flows through TxWatch

```
1. Poller wakes up (every poll_interval_seconds)
2. For each contract:
   a. GET /accounts/{contract_id}/transactions?cursor={last_seen}&order=asc
   b. For each new transaction:
      i.  Advance cursor (even if enrichment fails)
      ii. GET /transactions/{hash}/operations
          → extract function_name from invoke_host_function ops
          → extract amount_stroops from payment ops
      iii. Build EnrichedTransaction
      iv.  Evaluate all AlertRules → Vec<AlertPayload>
      v.   POST each AlertPayload to webhook_url (retry up to 3×)
```

---

## Stellar testnet resources

| Resource | URL |
|---|---|
| Testnet Horizon | https://horizon-testnet.stellar.org |
| Stellar Expert (testnet) | https://stellar.expert/explorer/testnet |
| Stellar Laboratory | https://laboratory.stellar.org |
| Friendbot (fund testnet accounts) | https://friendbot.stellar.org |
| Soroban docs | https://developers.stellar.org/docs/build/smart-contracts/overview |

---

## Sister repos

| Repo | Description |
|---|---|
| [tx-watch-web](https://github.com/Tx-wats/web) | Web dashboard for alert history and contract management |
| [tx-watch-contracts](https://github.com/Tx-wats/contracts) | Example Soroban contracts to monitor with TxWatch |

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache-2.0](LICENSE)

## Handsoff notes

<!-- handsoff-issue-35 -->
- #35: HTTP timeouts for Horizon and webhooks are hard-coded to 15 seconds

<!-- handsoff-issue-38 -->
- #38: startup_log_fields test helper duplicates production logic instead of testing it
<!-- handsoff-issue-24 -->
- #24: No way to configure a custom Horizon URL per contract

<!-- handsoff-issue-26 -->
- #26: Allow a starting cursor or ledger instead of always starting from "now"
<!-- handsoff-issue-33 -->
- #33: http_connection_verbose is parsed but never used

<!-- handsoff-issue-34 -->
- #34: http_tcp_keepalive_secs = 0 does not disable keepalive
<!-- handsoff-issue-27 -->
- #27: A corrupt cursor_file silently resets every contract to "now"

<!-- handsoff-issue-28 -->
- #28: Ctrl-C waits for webhook retries to finish before shutting down
<!-- handsoff-issue-39 -->
- #39: Tests sleep in real time for back-offs, slowing the suite by many seconds

<!-- handsoff-issue-41 -->
- #41: FunctionCalled is case-sensitive while AdminFunctionCalled is case-insensitive

<!-- handsoff-issue-285 -->
- #285: Contract Events Cost One `getEvents` Call per Ledger
<!-- handsoff-issue-287 -->
- #287: A Debug Log Message Contains a Long Run of Embedded Spaces
