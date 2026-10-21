//! Integration tests for `validate --check-webhooks` (#224) and
//! `validate --check-horizon` (#225).

use std::{env, fs, path::PathBuf};

use tokio::process::Command;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── helpers ──────────────────────────────────────────────────────────────────

fn write_config_custom_network(name: &str, webhook_url: &str, horizon_url: &str) -> PathBuf {
    let cfg = env::temp_dir().join(format!("txwatch_valcheck_{name}.toml"));
    fs::write(
        &cfg,
        format!(
            r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Check Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = {{ horizon_url = "{horizon_url}" }}
webhook_url = "{webhook_url}"

  [[contracts.rules]]
  type = "AnyTransaction"
"#
        ),
    )
    .unwrap();
    cfg
}

fn write_config_custom_with_passphrase(
    name: &str,
    horizon_url: &str,
    passphrase: &str,
) -> PathBuf {
    let cfg = env::temp_dir().join(format!("txwatch_valcheck_{name}.toml"));
    fs::write(
        &cfg,
        format!(
            r#"
poll_interval_seconds = 10

[[contracts]]
label       = "PhraseCheck"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = {{ horizon_url = "{horizon_url}", passphrase = "{passphrase}" }}
webhook_url = "https://hooks.example.com/unused"

  [[contracts.rules]]
  type = "AnyTransaction"
"#
        ),
    )
    .unwrap();
    cfg
}

// ── #224: --check-webhooks status reporting ───────────────────────────────────

#[tokio::test]
async fn check_webhooks_reports_401_as_reachable_auth_required() {
    let receiver = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&receiver)
        .await;

    let config = write_config_custom_network(
        "webhook_401",
        &format!("{}/hook", receiver.uri()),
        "http://127.0.0.1:1",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "validate",
            "--check-webhooks",
        ])
        .output()
        .await
        .expect("failed to run txwatch");

    // 401 means reachable (auth required) — not a failure.
    assert!(
        output.status.success(),
        "401 should not be a failure; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("reachable (auth required)"),
        "expected 'reachable (auth required)' in: {stdout}"
    );
}

#[tokio::test]
async fn check_webhooks_reports_403_as_reachable_auth_required() {
    let receiver = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&receiver)
        .await;

    let config = write_config_custom_network(
        "webhook_403",
        &format!("{}/hook", receiver.uri()),
        "http://127.0.0.1:1",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "validate",
            "--check-webhooks",
        ])
        .output()
        .await
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "403 should not be a failure; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("reachable (auth required)"),
        "expected 'reachable (auth required)' in: {stdout}"
    );
}

#[tokio::test]
async fn check_webhooks_reports_actual_status_code_for_500() {
    let receiver = MockServer::start().await;
    // HEAD → 500, then OPTIONS fallback → also 500.
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&receiver)
        .await;
    Mock::given(method("OPTIONS"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&receiver)
        .await;

    let config = write_config_custom_network(
        "webhook_500",
        &format!("{}/hook", receiver.uri()),
        "http://127.0.0.1:1",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "validate",
            "--check-webhooks",
        ])
        .output()
        .await
        .expect("failed to run txwatch");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Must report actual status, not the old generic "method not allowed".
    assert!(
        stdout.contains("HTTP 500"),
        "expected 'HTTP 500' in: {stdout}"
    );
    assert!(
        !stdout.contains("method not allowed"),
        "must not say 'method not allowed': {stdout}"
    );
}

#[tokio::test]
async fn check_webhooks_method_not_allowed_falls_back_to_options() {
    let receiver = MockServer::start().await;
    // HEAD → 405, OPTIONS → 200 → "reachable (OPTIONS)"
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&receiver)
        .await;
    Mock::given(method("OPTIONS"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let config = write_config_custom_network(
        "webhook_options",
        &format!("{}/hook", receiver.uri()),
        "http://127.0.0.1:1",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "validate",
            "--check-webhooks",
        ])
        .output()
        .await
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "OPTIONS 200 fallback should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("reachable (OPTIONS)"),
        "expected 'reachable (OPTIONS)' in: {stdout}"
    );
}

// ── #225: --check-horizon passphrase verification ────────────────────────────

fn horizon_root_json(passphrase: &str) -> serde_json::Value {
    serde_json::json!({
        "network_passphrase": passphrase,
        "core_latest_ledger": 100u64,
    })
}

fn horizon_found_json() -> serde_json::Value {
    serde_json::json!({ "_embedded": { "records": [] } })
}

#[tokio::test]
async fn check_horizon_passes_when_passphrase_matches() {
    let horizon = MockServer::start().await;
    // Root (GET /) returns the correct testnet passphrase.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(horizon_root_json(
                "Test SDF Network ; September 2015",
            )),
        )
        .mount(&horizon)
        .await;
    // Contract account query — returns contract found.
    Mock::given(method("GET"))
        .and(path_regex(r"^/accounts/.*/transactions$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(horizon_found_json()))
        .mount(&horizon)
        .await;

    let config = write_config_custom_with_passphrase(
        "passphrase_ok",
        &horizon.uri(),
        "Test SDF Network ; September 2015",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "validate",
            "--check-horizon",
        ])
        .output()
        .await
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "matching passphrase should pass; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn check_horizon_fails_when_passphrase_mismatches() {
    let horizon = MockServer::start().await;
    // Horizon reports mainnet passphrase, but config expects testnet.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(horizon_root_json(
                "Public Global Stellar Network ; September 2015",
            )),
        )
        .mount(&horizon)
        .await;

    let config = write_config_custom_with_passphrase(
        "passphrase_mismatch",
        &horizon.uri(),
        "Test SDF Network ; September 2015",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "validate",
            "--check-horizon",
        ])
        .output()
        .await
        .expect("failed to run txwatch");

    assert_eq!(
        output.status.code(),
        Some(1),
        "passphrase mismatch must fail; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("passphrase mismatch"),
        "expected passphrase mismatch message in: {stdout}"
    );
}

#[tokio::test]
async fn check_horizon_skips_passphrase_check_when_not_configured() {
    // Custom network without `passphrase` — the check is skipped entirely.
    let horizon = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(horizon_root_json("Some Random Network")),
        )
        .mount(&horizon)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/accounts/.*/transactions$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(horizon_found_json()))
        .mount(&horizon)
        .await;

    let config = write_config_custom_network(
        "passphrase_skip",
        "https://hooks.example.com/unused",
        &horizon.uri(),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "validate",
            "--check-horizon",
        ])
        .output()
        .await
        .expect("failed to run txwatch");

    // No passphrase configured → check skipped → pass.
    assert!(
        output.status.success(),
        "no passphrase configured: check should be skipped; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
