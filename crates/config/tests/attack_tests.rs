//! Attack tests for `txwatch-config` verifying defenses against malicious configurations,
//! integer overflow attempts, injection attacks, and invalid contract parameters.

use txwatch_config::{AlertRule, RuleConfig, WatchedContract};

#[test]
fn test_attack_oversized_transfer_threshold_rejected() {
    let mut rule = AlertRule::LargeTransfer {
        threshold_xlm: 2_000_000_000, // Exceeds MAX_LARGE_TRANSFER_THRESHOLD_XLM (1_000_000_000)
        threshold_stroops: 0,         // Filled in by `validate`
        direction: "any".into(),
    };
    let result = rule.validate("attack-target");
    assert!(
        result.is_err(),
        "Validator must reject absurdly large threshold exceeding bound"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("LargeTransfer threshold_xlm must be <="));
}

#[test]
fn test_attack_zero_threshold_rejected() {
    let mut rule = AlertRule::LargeTransfer {
        threshold_xlm: 0,
        threshold_stroops: 0,
        direction: "any".into(),
    };
    let result = rule.validate("attack-target");
    assert!(result.is_err(), "Validator must reject zero threshold");
}

#[test]
fn test_attack_symbol_injection_rejected() {
    let mut rule = AlertRule::FunctionCalled {
        function_name: "drop table users;--".into(),
        match_mode: Default::default(),
    };
    let result = rule.validate("attack-target");
    assert!(
        result.is_err(),
        "Validator must reject SQL/command-like special characters in Soroban symbol"
    );

    let mut rule_long = AlertRule::FunctionCalled {
        function_name: "a".repeat(33), // Max 32 chars
        match_mode: Default::default(),
    };
    let result_long = rule_long.validate("attack-target");
    assert!(
        result_long.is_err(),
        "Validator must reject symbols longer than 32 characters"
    );
}

#[test]
fn test_attack_contract_address_forgery_rejected() {
    let mut contract = WatchedContract {
        label: "Malicious Contract".into(),
        contract_id: "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(), // Starts with 'G' (account), not 'C' (contract)
        network: txwatch_config::Network::Testnet,
        rules: vec![RuleConfig {
            rule: AlertRule::AnyTransaction,
            cooldown_seconds: None,
            cooldown_summary: false,
        }],
        webhook_url: Some("https://example.com/webhook".into()),
        webhook_secret: None,
        webhook_format: Default::default(),
        webhook_headers: Default::default(),
        webhook_routing_key: None,
        webhooks: vec![],
        poll_interval_seconds: None,
        enabled: true,
        soroban_rpc_url: None,
        batch_alerts: false,
        horizon_base_url_override: None,
    };
    let result = contract.validate();
    assert!(
        result.is_err(),
        "Validator must reject non-C contract address prefixes"
    );
}

#[test]
fn test_attack_control_character_injection_rejected() {
    let mut contract = WatchedContract {
        label: "Exploit\r\nHeader-Injection: true".into(),
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        network: txwatch_config::Network::Testnet,
        rules: vec![RuleConfig {
            rule: AlertRule::AnyTransaction,
            cooldown_seconds: None,
            cooldown_summary: false,
        }],
        webhook_url: Some("https://example.com/webhook".into()),
        webhook_secret: None,
        webhook_format: Default::default(),
        webhook_headers: Default::default(),
        webhook_routing_key: None,
        webhooks: vec![],
        poll_interval_seconds: None,
        enabled: true,
        soroban_rpc_url: None,
        batch_alerts: false,
        horizon_base_url_override: None,
    };
    let result = contract.validate();
    assert!(
        result.is_err(),
        "Validator must reject control character injection in label"
    );
}
