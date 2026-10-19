//! Attack tests for `txwatch-rules` verifying defenses against numeric overflow,
//! symbol spoofing, and evasion attacks.

use txwatch_config::{AlertRule, RuleConfig, RuleEntry};
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
