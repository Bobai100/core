use std::{env, fs, process::Command};

fn txwatch_bin() -> Command {
    // `cargo test` sets CARGO_BIN_EXE_txwatch when the binary is declared in the same workspace.
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

const MULTI_CONTRACT_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Alpha Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/alpha"

  [[contracts.rules]]
  type = "AnyTransaction"

[[contracts]]
label       = "Beta Contract"
contract_id = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526"
network     = "mainnet"
webhook_url = "https://hooks.example.com/beta"

  [[contracts.rules]]
  type = "TransactionFailed"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

#[test]
fn validate_exits_zero_for_valid_config() {
    let dir = env::temp_dir();
    let path = dir.join("txwatch_valid_test.toml");
    fs::write(&path, VALID_CONFIG).unwrap();

    let status = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .status()
        .expect("failed to run txwatch");

    assert!(status.success(), "expected exit code 0 for valid config");
}

#[test]
fn validate_prints_all_contract_labels_ids_and_rule_counts() {
    let dir = env::temp_dir();
    let path = dir.join("txwatch_validate_labels_test.toml");
    fs::write(&path, MULTI_CONTRACT_CONFIG).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "expected exit code 0 for valid config"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Both contract labels must appear.
    assert!(
        stdout.contains("Alpha Contract"),
        "expected 'Alpha Contract' label in output"
    );
    assert!(
        stdout.contains("Beta Contract"),
        "expected 'Beta Contract' label in output"
    );

    // Both contract IDs must appear.
    assert!(
        stdout.contains("CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"),
        "expected Alpha contract_id in output"
    );
    assert!(
        stdout.contains("CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526"),
        "expected Beta contract_id in output"
    );

    // Rule counts: Alpha has 1 rule, Beta has 2 rules.
    assert!(
        stdout.contains("rules        : 1"),
        "expected rule count 1 for Alpha"
    );
    assert!(
        stdout.contains("rules        : 2"),
        "expected rule count 2 for Beta"
    );
}

#[test]
fn validate_output_includes_rule_label_snapshot() {
    const SNAPSHOT_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Test Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
  [[contracts.rules]]
  type = "TransactionFailed"
"#;

    let dir = env::temp_dir();
    let path = dir.join("txwatch_validate_snapshot_test.toml");
    fs::write(&path, SNAPSHOT_CONFIG).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "expected exit code 0 for valid config"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = concat!(
        "Config is valid.\n",
        "  poll_interval_seconds : 10\n",
        "  contracts             : 1\n",
        "\n",
        "  [Stellar Testnet] Test Contract\n",
        "    contract_id  : CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4\n",
        "    webhooks     : 1\n",
        "      - https://hooks.example.com/test (format: txwatch, secret: none)\n",
        "    interval     : 10s\n",
        "    rules        : 2\n",
        "      - AnyTransaction\n",
        "      - TransactionFailed\n",
        "    horizon      : https://horizon-testnet.stellar.org\n",
        "    explorer     : https://stellar.expert/explorer/testnet/contract/CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4\n"
    );

    assert_eq!(stdout, expected);
}

#[test]
fn validate_output_shows_horizon_url_override() {
    const OVERRIDE_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Test Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

    let path = env::temp_dir().join("txwatch_validate_horizon_override_test.toml");
    fs::write(&path, OVERRIDE_CONFIG).unwrap();

    let output = txwatch_bin()
        .args([
            "--config",
            path.to_str().unwrap(),
            "--horizon-url",
            "https://horizon.internal.example.com",
            "validate",
        ])
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "expected exit code 0 for valid config"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = concat!(
        "Config is valid.\n",
        "  poll_interval_seconds : 10\n",
        "  contracts             : 1\n",
        "\n",
        "  [Stellar Testnet] Test Contract\n",
        "    contract_id  : CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4\n",
        "    webhooks     : 1\n",
        "      - https://hooks.example.com/test (format: txwatch, secret: none)\n",
        "    interval     : 10s\n",
        "    rules        : 1\n",
        "      - AnyTransaction\n",
        "    horizon      : https://horizon.internal.example.com\n",
        "    explorer     : https://stellar.expert/explorer/testnet/contract/CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4\n"
    );

    assert_eq!(stdout, expected);
    assert!(
        !stdout.contains("https://horizon-testnet.stellar.org"),
        "public horizon endpoint must not appear when --horizon-url is set:\n{}",
        stdout
    );
}

#[test]
fn validate_exits_one_for_invalid_config() {
    let dir = env::temp_dir();
    let path = dir.join("txwatch_invalid_test.toml");
    fs::write(&path, "this is not valid toml = = =").unwrap();

    let status = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .status()
        .expect("failed to run txwatch");

    assert_eq!(
        status.code(),
        Some(1),
        "expected exit code 1 for invalid config"
    );
}

#[test]
fn validate_prints_every_error_and_exits_one() {
    const MULTI_ERROR_CONFIG: &str = r#"
poll_interval_seconds = 1

[[contracts]]
label       = "Alpha"
contract_id = "CSHORT"
network     = "testnet"
webhook_url = "ftp://hooks.example.com/alpha"

  [[contracts.rules]]
  type          = "LargeTransfer"
  threshold_xlm = 0
"#;

    let path = env::temp_dir().join("txwatch_validate_multi_error_test.toml");
    fs::write(&path, MULTI_ERROR_CONFIG).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .output()
        .expect("failed to run txwatch");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "4 configuration errors:",
        "  - poll_interval_seconds must be >= 5",
        "  - contract 'Alpha': contract_id 'CSHORT' is not a valid Stellar contract address",
        "  - contract 'Alpha': webhook_url 'ftp://hooks.example.com/alpha' must use http or https scheme",
        "  - contract 'Alpha': LargeTransfer threshold_xlm must be > 0",
    ] {
        assert!(stderr.contains(expected), "missing {:?} in:\n{}", expected, stderr);
    }
}

#[test]
fn validate_output_shows_effective_poll_interval_per_contract() {
    const OVERRIDE_CONFIG: &str = r#"
[[contracts]]
label                 = "Fast"
contract_id           = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network               = "testnet"
webhook_url           = "https://hooks.example.com/fast"
poll_interval_seconds = 5

  [[contracts.rules]]
  type = "AnyTransaction"

[[contracts]]
label       = "Default"
contract_id = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526"
network     = "testnet"
webhook_url = "https://hooks.example.com/default"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

    let path = env::temp_dir().join("txwatch_validate_interval_test.toml");
    fs::write(&path, OVERRIDE_CONFIG).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "expected exit code 0 for valid config"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("  poll_interval_seconds : 10\n"),
        "{}",
        stdout
    );
    assert!(
        stdout.contains("    interval     : 5s (override)\n"),
        "{}",
        stdout
    );
    assert!(stdout.contains("    interval     : 10s\n"), "{}", stdout);
}

#[test]
fn validate_json_output_snapshot() {
    const JSON_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label          = "Test Contract"
contract_id    = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network        = "testnet"
webhook_url    = "https://hooks.example.com/test"
webhook_secret = "super-secret-value"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

    let dir = env::temp_dir();
    let path = dir.join("txwatch_validate_json_snapshot_test.toml");
    fs::write(&path, JSON_CONFIG).unwrap();

    let output = txwatch_bin()
        .args([
            "--config",
            path.to_str().unwrap(),
            "validate",
            "--format",
            "json",
        ])
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "expected exit code 0 for valid config"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("super-secret-value"),
        "webhook secret must be redacted"
    );
    let expected = r#"{
  "contracts": [
    {
      "contract_id": "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
      "enabled": true,
      "explorer_url": "https://stellar.expert/explorer/testnet/contract/CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
      "horizon_url": "https://horizon-testnet.stellar.org",
      "label": "Test Contract",
      "network": "testnet",
      "poll_interval_seconds": 10,
      "rules": [
        {
          "type": "AnyTransaction"
        }
      ],
      "webhook_secret_set": true,
      "webhook_url": "https://hooks.example.com/test",
      "webhooks": [
        {
          "format": "txwatch",
          "headers": {},
          "routing_key_set": false,
          "secret_set": true,
          "url": "https://hooks.example.com/test"
        }
      ]
    }
  ],
  "cursor_file": null,
  "poll_interval_seconds": 10,
  "valid": true
}
"#;
    assert_eq!(stdout, expected);
}

#[test]
fn validate_json_snapshot_with_cooldowns_and_horizon_override() {
    const COOLDOWN_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Cooldown Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"

  [[contracts.rules]]
  type            = "LargeTransfer"
  threshold_xlm   = 100
  cooldown_seconds = 60
"#;

    let dir = env::temp_dir();
    let path = dir.join("txwatch_validate_json_cooldown_test.toml");
    fs::write(&path, COOLDOWN_CONFIG).unwrap();

    let output = txwatch_bin()
        .args([
            "--config",
            path.to_str().unwrap(),
            "--horizon-url",
            "https://custom-horizon.example.com",
            "validate",
            "--format",
            "json",
        ])
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "expected exit code 0 for valid config"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = r#"{
  "contracts": [
    {
      "contract_id": "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
      "enabled": true,
      "explorer_url": "https://stellar.expert/explorer/testnet/contract/CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
      "horizon_url": "https://custom-horizon.example.com",
      "label": "Cooldown Contract",
      "network": "testnet",
      "poll_interval_seconds": 10,
      "rules": [
        {
          "type": "AnyTransaction"
        },
        {
          "cooldown_seconds": 60,
          "threshold_xlm": 100.0,
          "type": "LargeTransfer"
        }
      ],
      "webhook_secret_set": false,
      "webhook_url": "https://hooks.example.com/test",
      "webhooks": [
        {
          "format": "txwatch",
          "headers": {},
          "routing_key_set": false,
          "secret_set": false,
          "url": "https://hooks.example.com/test"
        }
      ]
    }
  ],
  "cursor_file": null,
  "poll_interval_seconds": 10,
  "valid": true
}
"#;
    assert_eq!(stdout, expected);

    // The JSON object must not contain duplicate keys; serde_json's map keeps
    // only the last value, so verify the effective override is reported.
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        json["contracts"][0]["horizon_url"],
        "https://custom-horizon.example.com"
    );
    let rules = json["contracts"][0]["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[1]["cooldown_seconds"], 60);
}

#[test]
fn validate_json_reports_errors() {
    let dir = env::temp_dir();
    let path = dir.join("txwatch_validate_json_invalid_test.toml");
    fs::write(&path, "this is not valid toml = = =").unwrap();

    let output = txwatch_bin()
        .args([
            "--config",
            path.to_str().unwrap(),
            "validate",
            "--format",
            "json",
        ])
        .output()
        .expect("failed to run txwatch");

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 for invalid config"
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be a JSON object");
    assert_eq!(json["valid"], false);
    assert!(json["error"]
        .as_str()
        .unwrap()
        .contains("failed to parse config file"));
}

const MULTI_DESTINATION_CONFIG: &str = r#"
[[contracts]]
label          = "Vault"
contract_id    = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network        = "testnet"
webhook_url    = "https://internal.example.com/hook"
webhook_headers = { "Authorization" = "Bearer header-secret-value" }

  [[contracts.webhooks]]
  url    = "https://hooks.slack.com/services/T/B/X"
  format = "slack"

  [[contracts.webhooks]]
  url         = "https://events.pagerduty.com/v2/enqueue"
  format      = "pagerduty"
  routing_key = "routing-key-secret-value"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

#[test]
fn validate_lists_every_destination_with_secrets_redacted() {
    let path = env::temp_dir().join("txwatch_validate_destinations_test.toml");
    fs::write(&path, MULTI_DESTINATION_CONFIG).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .output()
        .expect("failed to run txwatch");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = concat!(
        "    webhooks     : 3\n",
        "      - https://internal.example.com/hook (format: txwatch, secret: none, headers: Authorization: <redacted>)\n",
        "      - https://hooks.slack.com/services/T/B/X (format: slack, secret: none)\n",
        "      - https://events.pagerduty.com/v2/enqueue (format: pagerduty, secret: none, routing_key: <redacted>)\n",
    );
    assert!(stdout.contains(expected), "got:\n{}", stdout);
    assert!(
        !stdout.contains("header-secret-value"),
        "header value leaked"
    );
    assert!(
        !stdout.contains("routing-key-secret-value"),
        "routing key leaked"
    );
}

#[test]
fn validate_json_lists_every_destination_with_secrets_redacted() {
    let path = env::temp_dir().join("txwatch_validate_destinations_json_test.toml");
    fs::write(&path, MULTI_DESTINATION_CONFIG).unwrap();

    let output = txwatch_bin()
        .args([
            "--config",
            path.to_str().unwrap(),
            "validate",
            "--format",
            "json",
        ])
        .output()
        .expect("failed to run txwatch");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("header-secret-value"),
        "header value leaked"
    );
    assert!(
        !stdout.contains("routing-key-secret-value"),
        "routing key leaked"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let webhooks = json["contracts"][0]["webhooks"].as_array().unwrap();
    assert_eq!(webhooks.len(), 3);
    assert_eq!(webhooks[0]["headers"]["Authorization"], "<redacted>");
    assert_eq!(webhooks[1]["format"], "slack");
    assert_eq!(webhooks[2]["format"], "pagerduty");
    assert_eq!(webhooks[2]["routing_key_set"], true);
}
