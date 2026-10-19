//! Prometheus metrics for TxWatch (enabled with the `metrics` feature flag).
//!
//! Exposes, labelled by `contract` (label) and `network` where noted:
//! - `txwatch_transactions_total{contract,network}`      — transactions processed
//! - `txwatch_alerts_total{contract,network}`            — alert payloads sent (rules matched)
//! - `txwatch_alerts_suppressed_total{contract,network,rule_type}` — rule matches dropped by a cooldown
//! - `txwatch_transactions_skipped_total{contract,network}` — transactions dropped because they could not be enriched
//! - `txwatch_transactions_degraded_total{contract,network}` — transactions evaluated without operation details (enrichment failed after retries)
//! - `txwatch_webhook_failures_total{contract,network}`  — permanent webhook delivery failures
//! - `txwatch_event_fetch_failures_total{contract,reason}` — event fetches that failed or were unavailable
//! - `txwatch_horizon_request_duration_seconds{network}` — Horizon request latency
//! - `txwatch_horizon_rate_limited_total{contract,network}` — Horizon 429 (rate limited) responses
//! - `txwatch_webhook_delivery_duration_seconds`         — webhook delivery latency (incl. retries)
//! - `txwatch_webhook_deliveries_total{contract,format,destination,outcome}` — webhook deliveries;
//!   `destination` is the destination's index in the contract's list, never its URL
//! - `txwatch_webhook_delivery_duration_seconds{format,outcome}` — webhook delivery latency (incl. retries)
//! - `txwatch_last_successful_poll_timestamp_seconds{contract,network}` — data freshness
//! - `txwatch_consecutive_poll_failures{contract,network}` — current failure streak
//! - `txwatch_ledger_lag{contract,network}`              — ledgers between Horizon's latest ledger
//!   and the last processed transaction (0 when caught up)
//! - `txwatch_build_info{version,git_sha}`               — always 1
//!
//! An optional HTTP server can be started by calling [`serve_metrics`]:
//!
//! - `GET /metrics` — Prometheus text exposition format
//! - `GET /healthz` — `200` while the process is running
//! - `GET /readyz`  — `200` once a poll has succeeded within the last
//!   2 × `poll_interval_seconds`, `503` otherwise
//!
//! Any other path returns `404`, and any other method `405`.

use anyhow::{Context, Result};
use prometheus::{
    register_histogram_vec, register_int_counter_vec, register_int_gauge_vec, HistogramVec,
    IntCounterVec, IntGaugeVec,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use http_body_util::Full;
use hyper::{
    body::Bytes,
    header::{HeaderValue, CONTENT_TYPE},
    Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, sync::watch};

// ── Metrics ───────────────────────────────────────────────────────────────────

const CONTRACT_LABELS: &[&str] = &["contract", "network"];

fn transactions_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_transactions_total",
            "Total Stellar transactions processed, per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_transactions_total")
    })
}

fn transactions_skipped_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_transactions_skipped_total",
            "Total transactions skipped because they could not be enriched (e.g. an unparseable created_at), per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_transactions_skipped_total")
    })
}

fn transactions_degraded_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_transactions_degraded_total",
            "Total transactions evaluated without operation details because enrichment failed after retries, per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_transactions_degraded_total")
    })
}

fn alerts_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_alerts_total",
            "Total alert payloads sent (rules matched), per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_alerts_total")
    })
}

fn alerts_suppressed_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_alerts_suppressed_total",
            "Total rule matches suppressed by a cooldown, per watched contract and rule type",
            &["contract", "network", "rule_type"]
        )
        .expect("register txwatch_alerts_suppressed_total")
    })
}

fn webhook_failures_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_webhook_failures_total",
            "Total permanent webhook delivery failures (after all retries), per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_webhook_failures_total")
    })
}

fn horizon_rate_limited_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_horizon_rate_limited_total",
            "Total Horizon HTTP 429 (rate limited) responses, per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_horizon_rate_limited_total")
fn event_fetch_failures_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_event_fetch_failures_total",
            "Total event fetches that failed or were unavailable (e.g. outside the RPC retention window), per watched contract and reason",
            &["contract", "reason"]
        )
        .expect("register txwatch_event_fetch_failures_total")
    })
}

fn horizon_request_duration() -> &'static HistogramVec {
    static H: OnceLock<HistogramVec> = OnceLock::new();
    H.get_or_init(|| {
        register_histogram_vec!(
            "txwatch_horizon_request_duration_seconds",
            "Duration of Horizon HTTP requests",
            &["network"]
        )
        .expect("register txwatch_horizon_request_duration_seconds")
    })
}

fn webhook_deliveries_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_webhook_deliveries_total",
            "Total webhook deliveries (after retries), per contract, format, destination index and outcome",
            &["contract", "format", "destination", "outcome"]
        )
        .expect("register txwatch_webhook_deliveries_total")
    })
}

/// A delivery with retries can take close to a minute, beyond the default
/// buckets' 10 s ceiling.
const WEBHOOK_DELIVERY_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0,
];

fn webhook_delivery_duration() -> &'static HistogramVec {
    static H: OnceLock<HistogramVec> = OnceLock::new();
    H.get_or_init(|| {
        register_histogram_vec!(
            "txwatch_webhook_delivery_duration_seconds",
            "Duration of webhook deliveries, including retries",
            &["format", "outcome"],
            WEBHOOK_DELIVERY_BUCKETS.to_vec()
        )
        .expect("register txwatch_webhook_delivery_duration_seconds")
    })
}

fn last_successful_poll() -> &'static IntGaugeVec {
    static G: OnceLock<IntGaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        register_int_gauge_vec!(
            "txwatch_last_successful_poll_timestamp_seconds",
            "Unix time of the last successful poll, per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_last_successful_poll_timestamp_seconds")
    })
}

fn consecutive_poll_failures() -> &'static IntGaugeVec {
    static G: OnceLock<IntGaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        register_int_gauge_vec!(
            "txwatch_consecutive_poll_failures",
            "Consecutive failed polls, per watched contract (0 after a success)",
            CONTRACT_LABELS
        )
        .expect("register txwatch_consecutive_poll_failures")
    })
}

fn ledger_lag() -> &'static IntGaugeVec {
    static G: OnceLock<IntGaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        register_int_gauge_vec!(
            "txwatch_ledger_lag",
            "Ledgers between Horizon's latest ledger and the last processed transaction, per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_ledger_lag")
    })
}

/// Registers `txwatch_build_info{version,git_sha} 1`.
///
/// The build information is supplied by the caller (the CLI passes its own
/// compile-time version and git SHA), so the labels reflect the binary that
/// was actually built rather than this crate's compile-time environment.
pub fn register_build_info(version: &str, git_sha: &str) {
    static G: OnceLock<IntGaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        let gauge = register_int_gauge_vec!(
            "txwatch_build_info",
            "Build information; the value is always 1",
            &["version", "git_sha"]
        )
        .expect("register txwatch_build_info");
        gauge.with_label_values(&[version, git_sha]).set(1);
        gauge
    });
}

/// Increment `txwatch_transactions_total` for a contract by `n`.
pub fn inc_transactions(contract: &str, network: &str, n: u64) {
    transactions_total()
        .with_label_values(&[contract, network])
        .inc_by(n);
}

/// Increment `txwatch_transactions_skipped_total` for a contract by `n`.
/// Calling it with `0` creates the series so it is exported before any skip.
pub fn inc_transactions_skipped(contract: &str, network: &str, n: u64) {
    transactions_skipped_total()
        .with_label_values(&[contract, network])
        .inc_by(n);
}

/// Increment `txwatch_transactions_degraded_total` for a contract by `n`.
/// Calling it with `0` creates the series so it is exported before any degradation.
pub fn inc_transactions_degraded(contract: &str, network: &str, n: u64) {
    transactions_degraded_total()
        .with_label_values(&[contract, network])
        .inc_by(n);
}

/// Increment `txwatch_alerts_total` for a contract by `n`.
pub fn inc_alerts(contract: &str, network: &str, n: u64) {
    alerts_total()
        .with_label_values(&[contract, network])
        .inc();
}

/// Increment `txwatch_alerts_suppressed_total` for a contract's rule type by 1.
pub fn inc_alerts_suppressed(contract: &str, network: &str, rule_type: &str) {
    alerts_suppressed_total()
        .with_label_values(&[contract, network, rule_type])
        .inc();
}

/// Increment `txwatch_webhook_failures_total` for a contract by 1.
pub fn inc_webhook_failures(contract: &str, network: &str) {
    webhook_failures_total()
        .with_label_values(&[contract, network])
        .inc();
}

/// Increment `txwatch_horizon_rate_limited_total` for a contract by 1.
pub fn inc_horizon_rate_limited(contract: &str, network: &str) {
    horizon_rate_limited_total()
        .with_label_values(&[contract, network])
/// Increment `txwatch_event_fetch_failures_total{contract,reason}` by 1.
///
/// `reason` is a short, low-cardinality tag such as `"retention"` (the
/// requested ledger is older than RPC's `oldestLedger`), `"rpc"` (the
/// `getEvents` call itself failed), or `"cursor"` (the cursor was reset).
pub fn inc_event_fetch_failure(contract: &str, reason: &str) {
    event_fetch_failures_total()
        .with_label_values(&[contract, reason])
        .inc();
}

/// Record how long a Horizon request on `network` took.
pub fn observe_horizon_request(network: &str, seconds: f64) {
    horizon_request_duration()
        .with_label_values(&[network])
        .observe(seconds);
}

/// Record one webhook delivery (including retries): count it and its duration.
/// `destination` is the destination's index, never its URL.
pub fn record_webhook_delivery(
    contract: &str,
    format: &str,
    destination: &str,
    success: bool,
    seconds: f64,
) {
    let outcome = if success { "success" } else { "failure" };
    webhook_deliveries_total()
        .with_label_values(&[contract, format, destination, outcome])
        .inc();
    webhook_delivery_duration()
        .with_label_values(&[format, outcome])
        .observe(seconds);
}

/// Set `txwatch_ledger_lag` for a contract.
pub fn set_ledger_lag(contract: &str, network: &str, lag: u64) {
    ledger_lag()
        .with_label_values(&[contract, network])
        .set(lag as i64);
}

/// Delete the per-contract gauges of a contract that is no longer polled, so
/// freshness alerts stop firing for it.
pub fn remove_contract(contract: &str, network: &str) {
    for gauge in [
        last_successful_poll(),
        consecutive_poll_failures(),
        ledger_lag(),
    ] {
        let _ = gauge.remove_label_values(&[contract, network]);
    }
}

/// Record a successful poll of a contract: freshness timestamp and reset
/// failure streak.
pub fn record_poll_success(contract: &str, network: &str) {
    last_successful_poll()
        .with_label_values(&[contract, network])
        .set(now_secs() as i64);
    consecutive_poll_failures()
        .with_label_values(&[contract, network])
        .set(0);
}

/// Record a failed poll of a contract.
pub fn record_poll_failure(contract: &str, network: &str) {
    consecutive_poll_failures()
        .with_label_values(&[contract, network])
        .inc();
}

// ── Readiness ─────────────────────────

/* … truncated 4651 chars — edit only what you need near the top … */
