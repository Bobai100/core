//! Attack tests for `txwatch-config` verifying defenses against malicious configurations,
//! integer overflow attempts, injection attacks, and invalid contract parameters.

use txwatch_config::{AlertRule, RuleConfig, WatchedContract};

#[test]
fn test_attack_oversized_transfer_threshold_rejected() {
    let mut rule = AlertRule::LargeTransfer {
        threshold_xlm: 2_000_000_000, // Exceeds MAX_LARGE_TRANSFER_THRESHOLD_XLM (1_000_000_000)
        threshold_stroops: 0,         // Filled in by `validate`
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
    };
    let result = rule.validate("attack-target");
    assert!(result.is_err(), "Validator must reject zero threshold");
}

#[test]
fn test_attack_symbol_injection_rejected() {
    let mut rule = AlertRule::FunctionCalled {
        function_name: "drop table users;--".into(),
        match_mode: Default::default(),
        args: vec![],
    };
    let result = rule.validate("attack-target");
    assert!(
        result.is_err(),
        "Validator must reject SQL/command-like special characters in Soroban symbol"
    );

    let mut rule_long = AlertRule::FunctionCalled {
        function_name: "a".repeat(33), // Max 32 chars
        match_mode: Default::default(),
        args: vec![],
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

#[test]
fn test_attack_function_args_invalid_index_rejected() {
    let mut rule = AlertRule::FunctionCalled {
        function_name: "transfer".into(),
        match_mode: Default::default(),
        args: vec![txwatch_config::ArgMatcher {
            index: 0, // Argument indices are 1-based
            eq: Some("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into()),
            prefix: None,
        }],
    };
    let result = rule.validate("attack-target");
    assert!(
        result.is_err(),
        "Validator must reject zero argument index"
    );

    let mut rule_far = AlertRule::FunctionCalled {
        function_name: "transfer".into(),
        match_mode: Default::default(),
        args: vec![txwatch_config::ArgMatcher {
            index: 65, // Exceeds MAX_FUNCTION_ARG_INDEX
            eq: None,
            prefix: Some("G".into()),
        }],
    };
    let result_far = rule_far.validate("attack-target");
    assert!(
        result_far.is_err(),
        "Validator must reject out-of-range argument index"
    );
}

#[test]
fn test_attack_function_args_both_or_neither_matcher_rejected() {
    let mut rule_both = AlertRule::FunctionCalled {
        function_name: "transfer".into(),
        match_mode: Default::default(),
        args: vec![txwatch_config::ArgMatcher {
            index: 1,
            eq: Some("GABC".into()),
            prefix: Some("G".into()),
        }],
    };
    let result_both = rule_both.validate("attack-target");
    assert!(
        result_both.is_err(),
        "Validator must reject an argument matcher with both eq and prefix"
    );

    let mut rule_neither = AlertRule::FunctionCalled {
        function_name: "transfer".into(),
        match_mode: Default::default(),
        args: vec![txwatch_config::ArgMatcher {
            index: 1,
            eq: None,
            prefix: None,
        }],
    };
    let result_neither = rule_neither.validate("attack-target");
    assert!(
        result_neither.is_err(),
        "Validator must reject an argument matcher with neither eq nor prefix"
    );
}

#[test]
fn test_attack_rate_rule_invalid_parameters_rejected() {
    let mut rule_zero_count = AlertRule::Rate {
        rule: Box::new(AlertRule::AnyTransaction),
        count: 0,
        window_seconds: 300,
    };
    let result_zero_count = rule_zero_count.validate("attack-target");
    assert!(
        result_zero_count.is_err(),
        "Validator must reject a Rate rule with zero count"
    );

    let mut rule_zero_window = AlertRule::Rate {
        rule: Box::new(AlertRule::AnyTransaction),
        count: 10,
        window_seconds: 0,
    };
    let result_zero_window = rule_zero_window.validate("attack-target");
    assert!(
        result_zero_window.is_err(),
        "Validator must reject a Rate rule with zero window"
    );

    let mut rule_huge_window = AlertRule::Rate {
        rule: Box::new(AlertRule::AnyTransaction),
        count: 10,
        window_seconds: u64::MAX,
    };
    let result_huge_window = rule_huge_window.validate("attack-target");
    assert!(
        result_huge_window.is_err(),
        "Validator must reject a Rate rule with an out-of-range window"
    );
}

#[test]
fn test_attack_memo_rule_invalid_parameters_rejected() {
    let mut rule_both = AlertRule::Memo {
        memo_type: None,
        equals: Some("12345".into()),
        prefix: Some("12".into()),
    };
    let result_both = rule_both.validate("attack-target");
    assert!(
        result_both.is_err(),
        "Validator must reject a Memo rule with both equals and prefix"
    );

    let mut rule_neither = AlertRule::Memo {
        memo_type: None,
        equals: None,
        prefix: None,
    };
    let result_neither = rule_neither.validate("attack-target");
    assert!(
        result_neither.is_err(),
        "Validator must reject a Memo rule with neither equals nor prefix"
    );

    let mut rule_bad_type = AlertRule::Memo {
        memo_type: Some("bogus".into()),
        equals: Some("12345".into()),
        prefix: None,
    };
    let result_bad_type = rule_bad_type.validate("attack-target");
    assert!(
        result_bad_type.is_err(),
        "Validator must reject an unknown memo_type"
    );
}
