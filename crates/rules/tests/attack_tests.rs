//! Attack tests for `txwatch-rules` verifying defenses against numeric overflow,
//! symbol spoofing, and evasion attacks.

use txwatch_config::{AlertRule, DataMatcher, RuleConfig, TopicMatcher};
use txwatch_rules::{evaluate, EnrichedTransaction, EvalContext, HorizonTransaction};

fn dummy_tx(hash: &str, successful: bool, fee_charged: Option<&str>) -> HorizonTransaction {
    HorizonTransaction {
        hash: hash.into(),
        created_at: "2024-06-01T12:00:00Z".into(),
        successful,
        paging_token: "1000".into(),
        fee_charged: fee_charged.map(|s| s.into()),
        envelope_xdr: None,
        result_xdr: None,
        source_account: None,
        fee_account: None,
        ledger: None,
        memo: None,
        memo_type: None,
        operation_count: None,
    }
}

#[test]
fn test_attack_overflow_amount_handled_safely() {
    // An amount above the total XLM supply is not representable, so
    // `from_horizon` discards it rather than letting it drive an alert.
    let raw = dummy_tx("overflow_tx", true, None);
    let enriched =
        EnrichedTransaction::from_horizon(raw, vec!["transfer".into()], Some(u64::MAX), None)
            .unwrap();
    assert_eq!(
        enriched.amount_stroops, None,
        "an impossible amount must be discarded"
    );

    let rules = vec![RuleConfig {
        rule: AlertRule::LargeTransfer {
            threshold_xlm: 10_000,
            threshold_stroops: 100_000_000_000,
        },
        cooldown_seconds: None,
    }];
    let ctx = EvalContext {
        label: "Escrow",
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        network: "testnet",
        horizon_base: "https://horizon-testnet.stellar.org",
        explorer_base: Some("https://stellar.expert/explorer/testnet"),
    };
    let payloads = evaluate(&ctx, &rules, &enriched, None);

    assert!(
        payloads.is_empty(),
        "LargeTransfer must not fire on a discarded amount"
    );
}

#[test]
fn test_attack_case_spoofing_admin_function() {
    // Attack trying to bypass AdminFunctionCalled rule using mixed-case invocation
    let raw = dummy_tx("admin_tx", true, None);
    let enriched =
        EnrichedTransaction::from_horizon(raw, vec!["Set_Admin".into()], None, None).unwrap();

    let rules = vec![RuleConfig {
        rule: AlertRule::AdminFunctionCalled {
            function_names: vec!["set_admin".into(), "upgrade".into()],
        },
        cooldown_seconds: None,
    }];
    let ctx = EvalContext {
        label: "Escrow",
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        network: "testnet",
        horizon_base: "https://horizon-testnet.stellar.org",
        explorer_base: Some("https://stellar.expert/explorer/testnet"),
    };
    let payloads = evaluate(&ctx, &rules, &enriched, None);

    assert_eq!(
        payloads.len(),
        1,
        "AdminFunctionCalled must catch case variations of sensitive functions"
    );
}

#[test]
fn test_attack_status_spoofing_transaction_failed() {
    // Ensure successful transaction cannot trick TransactionFailed rule into firing
    let raw = dummy_tx("success_tx", true, None);
    let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();

    let rules = vec![RuleConfig {
        rule: AlertRule::TransactionFailed,
        cooldown_seconds: None,
    }];
    let ctx = EvalContext {
        label: "Escrow",
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        network: "testnet",
        horizon_base: "https://horizon-testnet.stellar.org",
        explorer_base: Some("https://stellar.expert/explorer/testnet"),
    };
    let payloads = evaluate(&ctx, &rules, &enriched, None);

    assert!(
        payloads.is_empty(),
        "TransactionFailed must never trigger on successful transactions"
    );
}

fn test_ctx() -> EvalContext<'static> {
    EvalContext {
        label: "Escrow",
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        network: "testnet",
        horizon_base: "https://horizon-testnet.stellar.org",
        explorer_base: Some("https://stellar.expert/explorer/testnet"),
    }
}

fn event_rule(topics: Vec<TopicMatcher>, data: Vec<DataMatcher>) -> Vec<RuleConfig> {
    vec![RuleConfig {
        rule: AlertRule::EventEmitted {
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            topics,
            data,
        },
        cooldown_seconds: None,
    }]
}

#[test]
fn test_attack_data_matcher_i128_integer_comparison() {
    // i128 values are compared as integers, not as strings or floats, so
    // large magnitudes near the i128 bounds still order correctly.
    let raw = dummy_tx("data_i128_tx", true, None);
    let mut enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
    enriched.events = vec![txwatch_rules::ContractEvent {
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        topics: vec![serde_json::json!({"symbol": "transfer"})],
        data: serde_json::json!({
            "amount": "170141183460469231731687303715884105727",
            "to": "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
        }),
    }];

    let rules = event_rule(
        vec![TopicMatcher::Symbol("transfer".into())],
        vec![DataMatcher {
            pointer: "/amount".into(),
            eq: None,
            gte: Some("100000000000000000000000000000000000000".into()),
            lte: None,
        }],
    );
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert_eq!(
        payloads.len(),
        1,
        "i128 data matcher must compare numerically, not lexically"
    );

    // A value just below the bound must not match.
    enriched.events[0].data = serde_json::json!({
        "amount": "99999999999999999999999999999999999999",
        "to": "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
    });
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert!(
        payloads.is_empty(),
        "i128 data matcher must not match values below the bound"
    );
}

#[test]
fn test_attack_data_matcher_u128_integer_comparison() {
    // u128 max must be handled without overflow when compared.
    let raw = dummy_tx("data_u128_tx", true, None);
    let mut enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
    enriched.events = vec![txwatch_rules::ContractEvent {
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        topics: vec![serde_json::json!({"symbol": "mint"})],
        data: serde_json::json!({
            "amount": "340282366920938463463374607431768211455"
        }),
    }];

    let rules = event_rule(
        vec![TopicMatcher::Symbol("mint".into())],
        vec![DataMatcher {
            pointer: "/amount".into(),
            eq: Some("340282366920938463463374607431768211455".into()),
            gte: None,
            lte: None,
        }],
    );
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert_eq!(
        payloads.len(),
        1,
        "u128 max must compare exactly as an integer"
    );
}

#[test]
fn test_attack_non_symbol_topic_zero_matches() {
    // Topic 0 that is a string (not a symbol) must still be matchable.
    let raw = dummy_tx("string_topic_tx", true, None);
    let mut enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
    enriched.events = vec![txwatch_rules::ContractEvent {
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        topics: vec![serde_json::json!({"string": "admin_changed"})],
        data: serde_json::json!({}),
    }];

    let rules = event_rule(vec![TopicMatcher::String("admin_changed".into())], vec![]);
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert_eq!(
        payloads.len(),
        1,
        "string topic 0 must be matchable without a symbol wrapper"
    );
}

#[test]
fn test_attack_typed_topic_pattern_matches() {
    // Typed patterns constrain by ScVal type as well as value.
    let raw = dummy_tx("typed_topic_tx", true, None);
    let mut enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
    enriched.events = vec![txwatch_rules::ContractEvent {
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        topics: vec![
            serde_json::json!({"symbol": "transfer"}),
            serde_json::json!({"u32": 5}),
        ],
        data: serde_json::json!({}),
    }];

    let rules = event_rule(
        vec![
            TopicMatcher::Symbol("transfer".into()),
            TopicMatcher::Typed {
                kind: "u32".into(),
                value: "5".into(),
            },
        ],
        vec![],
    );
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert_eq!(payloads.len(), 1, "typed u32 topic must match");

    // Same value but wrong type must not match.
    let rules = event_rule(
        vec![
            TopicMatcher::Symbol("transfer".into()),
            TopicMatcher::Typed {
                kind: "u64".into(),
                value: "5".into(),
            },
        ],
        vec![],
    );
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert!(
        payloads.is_empty(),
        "typed topic must not match a different ScVal type"
    );
}

#[test]
fn test_attack_large_transfer_direction_filters_unrelated_transfers() {
    // A transaction with many unrelated native payments must not trigger a
    // LargeTransfer for the watched contract when it moved nothing.
    let raw = dummy_tx("multi_op_tx", true, None);
    let mut enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
    enriched.amount_stroops = Some(0);
    enriched.contract_inflow_stroops = Some(0);
    enriched.contract_outflow_stroops = Some(0);

    let rules = vec![RuleConfig {
        rule: AlertRule::LargeTransfer {
            threshold_xlm: 10_000,
            threshold_stroops: 100_000_000_000,
            direction: "any".into(),
        },
        cooldown_seconds: None,
    }];
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert!(
        payloads.is_empty(),
        "unrelated transfers must not count toward the watched contract"
    );

    // Only the watched contract's outflow counts for direction = "out".
    enriched.contract_outflow_stroops = Some(200_000_000_000);
    let rules = vec![RuleConfig {
        rule: AlertRule::LargeTransfer {
            threshold_xlm: 10_000,
            threshold_stroops: 100_000_000_000,
            direction: "out".into(),
        },
        cooldown_seconds: None,
    }];
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert_eq!(
        payloads.len(),
        1,
        "outflow above threshold must fire for direction = out"
    );

    // Inflow-only movement must not fire for direction = "out".
    enriched.contract_outflow_stroops = Some(0);
    enriched.contract_inflow_stroops = Some(200_000_000_000);
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert!(
        payloads.is_empty(),
        "inflow must not fire for direction = out"
    );
}

#[test]
fn test_attack_contract_upgraded_reports_wasm_hashes() {
    // An executable change must fire ContractUpgraded with old/new hashes.
    let raw = dummy_tx("upgrade_tx", true, None);
    let mut enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
    enriched.contract_upgrade = Some(txwatch_rules::ContractUpgrade {
        old_wasm_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .into(),
        new_wasm_hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .into(),
    });

    let rules = vec![RuleConfig {
        rule: AlertRule::ContractUpgraded,
        cooldown_seconds: None,
    }];
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert_eq!(payloads.len(), 1, "ContractUpgraded must fire on upgrade");
    let payload = &payloads[0];
    assert_eq!(
        payload["old_wasm_hash"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(
        payload["new_wasm_hash"],
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    );

    // No upgrade recorded: rule must stay silent.
    enriched.contract_upgrade = None;
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert!(
        payloads.is_empty(),
        "ContractUpgraded must not fire without an executable change"
    );
#[test]
fn test_attack_rule_level_webhook_secret_only_override() {
    // A rule that sets only `webhook_secret` (no `webhook_url`) must still have
    // its secret applied to the relevant destination(s) rather than being
    // silently discarded by the `(None, _)` match arm.
    let raw = dummy_tx("secret_only_tx", false, None);
    let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();

    let rules = vec![RuleConfig {
        rule: AlertRule::TransactionFailed,
        cooldown_seconds: None,
    }];
    let ctx = EvalContext {
        label: "Escrow",
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        network: "testnet",
        horizon_base: "https://horizon-testnet.stellar.org",
        explorer_base: Some("https://stellar.expert/explorer/testnet"),
    };

    let payloads = evaluate(&ctx, &rules, &enriched, None);
    assert_eq!(payloads.len(), 1, "TransactionFailed must fire on failed tx");

    let payload = &payloads[0];
    let destinations = payload
        .destinations()
        .expect("payload must expose its destinations");
    assert!(
        !destinations.is_empty(),
        "contract destinations must be present when no rule override is set"
    );

    // Apply a secret-only override and confirm every destination carries the
    // rule-level secret instead of the contract-level one.
    let rule_secret = "rule-level-signing-key";
    let overridden = payload.with_webhook_secret_override(rule_secret);
    let overridden_destinations = overridden
        .destinations()
        .expect("overridden payload must expose its destinations");
    assert_eq!(
        overridden_destinations.len(),
        destinations.len(),
        "secret-only override must not change the destination set"
    );
    for dest in overridden_destinations {
        assert_eq!(
            dest.webhook_secret.as_deref(),
            Some(rule_secret),
            "rule-level webhook_secret must be applied to every destination"
        );
    }
}
