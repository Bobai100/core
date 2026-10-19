use std::{env, fs, process::Command};

fn txwatch_bin() -> Command {
    let bin = env!("CARGO_BIN_EXE_txwatch");
    Command::new(bin)
}

const VALID_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Test Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

#[test]
fn test_webhook_exits_one_when_url_is_unreachable() {
    let dir = env::temp_dir();
    let cfg_path = dir.join("txwatch_test_webhook_unreachable.toml");
    fs::write(&cfg_path, VALID_CONFIG).unwrap();

    // Port 1 on loopback is never open (requires root to bind), so the
    // connection is always refused — making this a reliable unreachable URL.
    let status = txwatch_bin()
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
        ])
        .status()
        .expect("failed to run txwatch");

    assert_eq!(
        status.code(),
        Some(1),
        "expected exit code 1 when webhook URL is unreachable"
    );
}

#[test]
fn test_webhook_does_not_require_a_config_file() {
    let status = txwatch_bin()
        .args([
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
            "--network",
            "testnet",
            "--secret",
            "local-secret",
        ])
        .status()
        .expect("failed to run txwatch");
    assert_eq!(status.code(), Some(1));
}

#[test]
fn test_webhook_rejects_horizon_url_without_scheme() {
    let output = txwatch_bin()
        .args([
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
            "--horizon-url",
            "horizon.example.com",
        ])
        .output()
        .expect("failed to run txwatch");

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 for a horizon URL without a scheme"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("horizon-url") || stderr.contains("horizon_url"),
        "expected a clear error mentioning the horizon URL, got: {stderr}"
    );
}

#[test]
fn test_webhook_rejects_non_http_horizon_url() {
    let output = txwatch_bin()
        .args([
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
            "--horizon-url",
            "ftp://horizon.example.com",
        ])
        .output()
        .expect("failed to run txwatch");

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 for a non-http horizon URL"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("horizon-url") || stderr.contains("horizon_url"),
        "expected a clear error mentioning the horizon URL, got: {stderr}"
    );
}

#[test]
fn test_webhook_accepts_public_and_pubnet_aliases() {
    for network in ["public", "pubnet"] {
        let output = txwatch_bin()
            .args([
                "test-webhook",
                "--url",
                "http://127.0.0.1:1/webhook",
                "--network",
                network,
                "--secret",
                "local-secret",
            ])
            .output()
            .expect("failed to run txwatch");

        // The webhook itself is unreachable, so we still expect exit code 1,
        // but the network value must not be rejected as invalid.
        assert_eq!(
            output.status.code(),
            Some(1),
            "expected exit code 1 (unreachable webhook) for network {network}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("invalid value") && !stderr.contains("possible values"),
            "network alias {network} should be accepted, got: {stderr}"
        );
    }
}

#[test]
fn test_webhook_custom_network_from_urls() {
    let output = txwatch_bin()
        .args([
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
            "--horizon-url",
            "https://horizon.local.example.com",
            "--explorer-url",
            "https://explorer.local.example.com",
            "--secret",
            "local-secret",
        ])
        .output()
        .expect("failed to run txwatch");

    // The webhook is unreachable, so exit code 1 is expected, but the custom
    // network URLs must be accepted rather than rejected as invalid.
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("invalid value") && !stderr.contains("possible values"),
        "custom network URLs should be accepted, got: {stderr}"
    );
}

#[test]
fn test_webhook_contract_lookup_is_case_insensitive() {
    let dir = env::temp_dir();
    let cfg_path = dir.join("txwatch_test_webhook_case_insensitive.toml");
    let config = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Vault"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;
    fs::write(&cfg_path, config).unwrap();

    let output = txwatch_bin()
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
            "--contract",
            "vault",
        ])
        .output()
        .expect("failed to run txwatch");

    // The webhook is unreachable, so exit code 1 is expected; the important
    // part is that the contract lookup by lowercase label succeeds.
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("not found"),
        "lowercase label should match contract labelled 'Vault', got: {stderr}"
    );
}

#[test]
fn test_webhook_contract_miss_lists_configured_labels() {
    let dir = env::temp_dir();
    let cfg_path = dir.join("txwatch_test_webhook_contract_miss.toml");
    let config = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Vault"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;
    fs::write(&cfg_path, config).unwrap();

    let output = txwatch_bin()
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
            "--contract",
            "does-not-exist",
        ])
        .output()
        .expect("failed to run txwatch");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Vault"),
        "expected the error to list configured labels, got: {stderr}"
    );
}
