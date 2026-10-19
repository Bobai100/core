//! Attack tests for `txwatch-rules` verifying defenses against numeric overflow,
//! symbol spoofing, and evasion attacks.

use chrono::{DateTime, Duration, Utc};
use txwatch_config::{AlertRule, ArgMatcher, DataMatcher, RuleConfig, RuleEntry, TopicMatcher};
use txwatch_rules::{
    evaluate, EnrichedTransaction, EvalContext, FunctionInvocation, HorizonTransaction,
    RateTracker,
};

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

    let rule_secret = "rule-level-signing-key";
    let rules = vec![RuleEntry {
        rule: AlertRule::TransactionFailed,
        webhook_url: None,
        webhook_secret: Some(rule_secret.to_string()),
        severity: None,
        enabled: true,
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
        "TransactionFailed must fire on failed tx"
    );

    let payload = &payloads[0];
    assert_eq!(
        payload.effective_webhook_secret.as_deref(),
        Some(rule_secret),
        "rule-level webhook_secret must be applied to payload even without webhook_url"
    );
}

// ── Issue #266: FunctionCalled argument matchers ─────────────────────────────

fn function_called(args: Vec<ArgMatcher>) -> RuleConfig {
    RuleConfig {
        rule: AlertRule::FunctionCalled {
            function_name: "transfer".into(),
            match_mode: Default::default(),
            args,
        },
        cooldown_seconds: None,
    }
}

fn enriched_with_args(hash: &str, function: &str, args: Vec<serde_json::Value>) -> EnrichedTransaction {
    let raw = dummy_tx(hash, true, None);
    EnrichedTransaction::from_horizon(raw, vec![function.into()], None, None)
        .unwrap()
        .with_invocations(vec![FunctionInvocation::from_parameters(
            function.into(),
            &args,
        )])
}

#[test]
fn test_function_called_arg_address_match() {
    let ctx = test_ctx();
    let to = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    let rules = vec![function_called(vec![ArgMatcher {
        index: 1,
        eq: to.into(),
    }])];

    let matching = enriched_with_args(
        "addr_match",
        "transfer",
        vec![
            serde_json::json!({ "i128": "100" }),
            serde_json::json!({ "address": to }),
        ],
    );
    let payloads = evaluate(&ctx, &rules, &matching, None);
    assert_eq!(payloads.len(), 1, "address argument must match");
    assert_eq!(
        payloads[0].function_args[0].arg(1),
        Some(to),
        "decoded argument values must be included in the payload"
    );

    let non_matching = enriched_with_args(
        "addr_miss",
        "transfer",
        vec![
            serde_json::json!({ "i128": "100" }),
            serde_json::json!({ "address": "GBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB" }),
        ],
    );
    assert!(
        evaluate(&ctx, &rules, &non_matching, None).is_empty(),
        "different address must not match"
    );
}

#[test]
fn test_function_called_arg_scalar_types_match() {
    let ctx = test_ctx();
    let rules = vec![function_called(vec![ArgMatcher {
        index: 0,
        eq: "42".into(),
    }])];

    for value in [
        serde_json::json!({ "u32": 42 }),
        serde_json::json!({ "i128": "42" }),
        serde_json::json!({ "symbol": "42" }),
        serde_json::json!({ "string": "42" }),
    ] {
        let tx = enriched_with_args("scalar_match", "transfer", vec![value.clone()]);
        assert_eq!(
            evaluate(&ctx, &rules, &tx, None).len(),
            1,
            "{value} must match"
        );
    }

    let miss = enriched_with_args("scalar_miss", "transfer", vec![serde_json::json!({ "u32": 43 })]);
    assert!(evaluate(&ctx, &rules, &miss, None).is_empty());
}

#[test]
fn test_function_called_arg_requires_decoded_invocation() {
    // Without decoded parameters an argument constraint cannot be proven, so
    // the rule must not fire on the function name alone.
    let ctx = test_ctx();
    let rules = vec![function_called(vec![ArgMatcher {
        index: 0,
        eq: "admin".into(),
    }])];
    let raw = dummy_tx("no_params", true, None);
    let tx = EnrichedTransaction::from_horizon(raw, vec!["transfer".into()], None, None).unwrap();
    assert!(evaluate(&ctx, &rules, &tx, None).is_empty());

    let missing_index = enriched_with_args("short_args", "transfer", vec![]);
    assert!(evaluate(&ctx, &rules, &missing_index, None).is_empty());
}

// ── Issue #267: Rate rules ───────────────────────────────────────────────────

fn rate_rule(count: u64, window_seconds: u64) -> Vec<RuleConfig> {
    vec![RuleConfig {
        rule: AlertRule::Rate {
            rule: Box::new(RuleEntry {
                enabled: true,
                webhook_url: None,
                webhook_secret: None,
                severity: None,
                rule: AlertRule::TransactionFailed,
            }),
            count,
            window_seconds,
        },
        cooldown_seconds: None,
    }]
}

fn failed(hash: &str) -> EnrichedTransaction {
    EnrichedTransaction::from_horizon(dummy_tx(hash, false, None), vec![], None, None).unwrap()
}

fn t0() -> DateTime<Utc> {
    "2024-06-01T12:00:00Z".parse().unwrap()
}

#[test]
fn test_rate_rule_threshold_and_window_expiry() {
    let ctx = test_ctx();
    let rules = rate_rule(3, 300);
    let mut rates = RateTracker::new();
    let now = t0();

    // Rate rules never fire through the stateless evaluator.
    assert!(evaluate(&ctx, &rules, &failed("f0"), None).is_empty());

    // Two failures: below threshold.
    assert!(rates.evaluate(&ctx, &rules, &failed("f1"), now).is_empty());
    assert!(rates.evaluate(&ctx, &rules, &failed("f2"), now).is_empty());

    // Third failure within the window breaches the threshold.
    let payloads = rates.evaluate(&ctx, &rules, &failed("f3"), now);
    assert_eq!(payloads.len(), 1, "one alert per breach");
    assert_eq!(payloads[0].observed_count, Some(3));
    assert_eq!(payloads[0].rule_type, "Rate");

    // A successful transaction does not count toward the rate.
    let ok = EnrichedTransaction::from_horizon(dummy_tx("ok1", true, None), vec![], None, None)
        .unwrap();
    assert!(rates
        .evaluate(&ctx, &rules, &ok, now + Duration::seconds(10))
        .is_empty());

    // The breach cleared the window; old failures also expire with time.
    let later = now + Duration::seconds(301);
    assert!(rates.evaluate(&ctx, &rules, &failed("f4"), later).is_empty());
    assert!(rates.evaluate(&ctx, &rules, &failed("f5"), later).is_empty());
    assert!(
        rates
            .evaluate(&ctx, &rules, &failed("f6"), later + Duration::seconds(300))
            .is_empty(),
        "failures outside the window must not count"
    );
}

#[test]
fn test_rate_rule_per_contract_isolation() {
    let rules = rate_rule(2, 60);
    let mut rates = RateTracker::new();
    let now = t0();

    let ctx_a = test_ctx();
    let mut ctx_b = test_ctx();
    ctx_b.contract_id = "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    assert!(rates.evaluate(&ctx_a, &rules, &failed("a1"), now).is_empty());
    // Contract B's first failure must not be influenced by contract A's state.
    assert!(rates.evaluate(&ctx_b, &rules, &failed("b1"), now).is_empty());
    // Contract A's second failure breaches its own window.
    assert_eq!(rates.evaluate(&ctx_a, &rules, &failed("a2"), now).len(), 1);
    // Contract B breaches only on its own second failure.
    assert_eq!(rates.evaluate(&ctx_b, &rules, &failed("b2"), now).len(), 1);
}

// ── Issue #268: Memo rules ───────────────────────────────────────────────────

fn memo_rule(memo_type: Option<&str>, equals: Option<&str>, prefix: Option<&str>) -> Vec<RuleConfig> {
    vec![RuleConfig {
        rule: AlertRule::Memo {
            memo_type: memo_type.map(Into::into),
            equals: equals.map(Into::into),
            prefix: prefix.map(Into::into),
        },
        cooldown_seconds: None,
    }]
}

#[test]
fn test_memo_rule_exact_and_prefix() {
    let ctx = test_ctx();
    let mut matching =
        EnrichedTransaction::from_horizon(dummy_tx("memo_exact", true, None), vec![], None, None)
            .unwrap();
    matching.memo = Some("EXCHANGE-123".into());
    matching.memo_type = Some("text".into());

    let exact = memo_rule(Some("text"), Some("EXCHANGE-123"), None);
    assert_eq!(evaluate(&ctx, &exact, &matching, None).len(), 1);

    let mut wrong_type = matching.clone();
    wrong_type.memo_type = Some("id".into());
    assert!(
        evaluate(&ctx, &exact, &wrong_type, None).is_empty(),
        "memo_type must match"
    );

    let prefix = memo_rule(None, None, Some("EXCHANGE-"));
    assert_eq!(evaluate(&ctx, &prefix, &matching, None).len(), 1);

    let mut non_matching = matching.clone();
    non_matching.memo = Some("OTHER-1".into());
    assert!(evaluate(&ctx, &prefix, &non_matching, None).is_empty());

    let mut no_memo = matching.clone();
    no_memo.memo = None;
    assert!(evaluate(&ctx, &prefix, &no_memo, None).is_empty());
}
