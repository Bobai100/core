//! Attack tests for `txwatch-rules` verifying defenses against numeric overflow,
//! symbol spoofing, and evasion attacks.

use std::cell::RefCell;

use txwatch_config::{AlertRule, ArgMatcher, MemoMatcher, MemoType, RuleConfig};
use txwatch_rules::{
    evaluate, evaluate_at, EnrichedTransaction, EvalContext, HorizonTransaction, ScVal,
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

fn test_ctx() -> EvalContext<'static> {
    EvalContext {
        label: "Escrow",
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        network: "testnet",
        horizon_base: "https://horizon-testnet.stellar.org",
        explorer_base: Some("https://stellar.expert/explorer/testnet"),
    }
}

fn function_called(args: Vec<ArgMatcher>) -> RuleConfig {
    RuleConfig {
        rule: AlertRule::FunctionCalled {
            function_names: vec!["transfer".into()],
            args,
        },
        cooldown_seconds: None,
    }
}

fn enriched_with_args(
    hash: &str,
    function: &str,
    args: Vec<ScVal>,
) -> EnrichedTransaction {
    let raw = dummy_tx(hash, true, None);
    let mut enriched =
        EnrichedTransaction::from_horizon(raw, vec![function.into()], None, None).unwrap();
    enriched.arguments = args;
    enriched
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

#[test]
fn test_function_called_arg_address_match() {
    let ctx = test_ctx();
    let rules = vec![function_called(vec![ArgMatcher {
        index: 1,
        eq: Some("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF".into()),
    }])];

    let matching = enriched_with_args(
        "addr_match",
        "transfer",
        vec![
            ScVal::Address("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF".into()),
            ScVal::I128(100),
        ],
    );
    let payloads = evaluate(&ctx, &rules, &matching, None);
    assert_eq!(payloads.len(), 1, "address argument must match");
    assert_eq!(
        payloads[0].arguments,
        vec![
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF".to_string(),
            "100".to_string()
        ],
        "argument values must be included in the payload"
    );

    let non_matching = enriched_with_args(
        "addr_miss",
        "transfer",
        vec![
            ScVal::Address("GBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".into()),
            ScVal::I128(100),
        ],
    );
    assert!(
        evaluate(&ctx, &rules, &non_matching, None).is_empty(),
        "different address must not match"
    );
}

#[test]
fn test_function_called_arg_symbol_match() {
    let ctx = test_ctx();
    let rules = vec![function_called(vec![ArgMatcher {
        index: 0,
        eq: Some("admin".into()),
    }])];

    let matching = enriched_with_args("sym_match", "transfer", vec![ScVal::Symbol("admin".into())]);
    assert_eq!(evaluate(&ctx, &rules, &matching, None).len(), 1);

    let non_matching =
        enriched_with_args("sym_miss", "transfer", vec![ScVal::Symbol("user".into())]);
    assert!(evaluate(&ctx, &rules, &non_matching, None).is_empty());
}

#[test]
fn test_function_called_arg_numeric_match() {
    let ctx = test_ctx();
    let rules = vec![function_called(vec![ArgMatcher {
        index: 0,
        eq: Some("42".into()),
    }])];

    let u32_match = enriched_with_args("u32_match", "transfer", vec![ScVal::U32(42)]);
    assert_eq!(evaluate(&ctx, &rules, &u32_match, None).len(), 1);

    let i128_match = enriched_with_args("i128_match", "transfer", vec![ScVal::I128(42)]);
    assert_eq!(evaluate(&ctx, &rules, &i128_match, None).len(), 1);

    let i128_miss = enriched_with_args("i128_miss", "transfer", vec![ScVal::I128(43)]);
    assert!(evaluate(&ctx, &rules, &i128_miss, None).is_empty());
}

#[test]
fn test_function_called_arg_string_match() {
    let ctx = test_ctx();
    let rules = vec![function_called(vec![ArgMatcher {
        index: 0,
        eq: Some("hello".into()),
    }])];

    let matching =
        enriched_with_args("str_match", "transfer", vec![ScVal::String("hello".into())]);
    assert_eq!(evaluate(&ctx, &rules, &matching, None).len(), 1);

    let non_matching =
        enriched_with_args("str_miss", "transfer", vec![ScVal::String("world".into())]);
    assert!(evaluate(&ctx, &rules, &non_matching, None).is_empty());
}

#[test]
fn test_function_called_any_argument() {
    let ctx = test_ctx();
    // `eq: None` matches any value at the given index.
    let rules = vec![function_called(vec![ArgMatcher {
        index: 0,
        eq: None,
    }])];

    let with_arg = enriched_with_args("any_arg", "transfer", vec![ScVal::U32(7)]);
    assert_eq!(evaluate(&ctx, &rules, &with_arg, None).len(), 1);

    let without_arg = enriched_with_args("no_arg", "transfer", vec![]);
    assert!(
        evaluate(&ctx, &rules, &without_arg, None).is_empty(),
        "missing argument must not match an 'any argument' matcher"
    );
}

#[test]
fn test_rate_rule_threshold_and_window_expiry() {
    let ctx = test_ctx();
    let rules = vec![RuleConfig {
        rule: AlertRule::Rate {
            rule: Box::new(AlertRule::TransactionFailed),
            count: 3,
            window_seconds: 300,
        },
        cooldown_seconds: None,
    }];

    let clock = RefCell::new(1_000u64);
    let mut now = || *clock.borrow();

    let failed = |hash: &str| {
        let raw = dummy_tx(hash, false, None);
        EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap()
    };

    // Two failures: below threshold.
    assert!(evaluate_at(&ctx, &rules, &failed("f1"), None, now()).is_empty());
    assert!(evaluate_at(&ctx, &rules, &failed("f2"), None, now()).is_empty());

    // Third failure within the window breaches the threshold.
    let payloads = evaluate_at(&ctx, &rules, &failed("f3"), None, now());
    assert_eq!(payloads.len(), 1, "one alert per breach");
    assert_eq!(payloads[0].observed_count, Some(3));

    // A successful transaction does not count toward the rate.
    *clock.borrow_mut() += 10;
    assert!(evaluate_at(&ctx, &rules, &dummy_ok("ok1"), None, now()).is_empty());

    // Advance past the window: earlier failures expire.
    *clock.borrow_mut() += 300;
    assert!(
        evaluate_at(&ctx, &rules, &failed("f4"), None, now()).is_empty(),
        "failures outside the window must not count"
    );
}

#[test]
fn test_rate_rule_per_contract_isolation() {
    let rules = vec![RuleConfig {
        rule: AlertRule::Rate {
            rule: Box::new(AlertRule::TransactionFailed),
            count: 2,
            window_seconds: 60,
        },
        cooldown_seconds: None,
    }];

    let ctx_a = test_ctx();
    let mut ctx_b = test_ctx();
    ctx_b.contract_id = "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    let clock = RefCell::new(5_000u64);
    let now = || *clock.borrow();

    let failed = |hash: &str| {
        let raw = dummy_tx(hash, false, None);
        EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap()
    };

    assert!(evaluate_at(&ctx_a, &rules, &failed("a1"), None, now()).is_empty());
    // Contract B's first failure must not be influenced by contract A's state.
    assert!(evaluate_at(&ctx_b, &rules, &failed("b1"), None, now()).is_empty());
    // Contract A's second failure breaches its own window.
    assert_eq!(evaluate_at(&ctx_a, &rules, &failed("a2"), None, now()).len(), 1);
    // Contract B still needs another failure.
    assert!(evaluate_at(&ctx_b, &rules, &failed("b2"), None, now()).is_empty());
}

#[test]
fn test_memo_rule_exact_and_prefix() {
    let ctx = test_ctx();
    let rules = vec![RuleConfig {
        rule: AlertRule::Memo {
            memo_type: Some(MemoType::Text),
            equals: Some("EXCHANGE-123".into()),
            prefix: None,
        },
        cooldown_seconds: None,
    }];

    let mut matching = EnrichedTransaction::from_horizon(
        dummy_tx("memo_exact", true, None),
        vec![],
        None,
        None,
    )
    .unwrap();
    matching.memo = Some("EXCHANGE-123".into());
    matching.memo_type = Some(MemoType::Text);
    assert_eq!(evaluate(&ctx, &rules, &matching, None).len(), 1);

    let mut wrong_type = matching.clone();
    wrong_type.memo_type = Some(MemoType::Id);
    assert!(
        evaluate(&ctx, &rules, &wrong_type, None).is_empty(),
        "memo_type must match"
    );

    let prefix_rules = vec![RuleConfig {
        rule: AlertRule::Memo {
            memo_type: None,
            equals: None,
            prefix: Some("EXCHANGE-".into()),
        },
        cooldown_seconds: None,
    }];
    assert_eq!(evaluate(&ctx, &prefix_rules, &matching, None).len(), 1);

    let mut non_matching = matching.clone();
    non_matching.memo = Some("OTHER-1".into());
    assert!(evaluate(&ctx, &prefix_rules, &non_matching, None).is_empty());
}
