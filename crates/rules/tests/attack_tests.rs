//! Attack tests for `txwatch-rules` verifying defenses against numeric overflow,
//! symbol spoofing, and evasion attacks.

use txwatch_config::{AlertRule, RuleConfig};
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

fn test_ctx() -> EvalContext<'static> {
    EvalContext {
        label: "Escrow",
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        network: "testnet",
        horizon_base: "https://horizon-testnet.stellar.org",
        explorer_base: Some("https://stellar.expert/explorer/testnet"),
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
            direction: "any".into(),
        },
        cooldown_seconds: None,
        cooldown_summary: false,
    }];
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);

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
        cooldown_summary: false,
    }];
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);

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
        cooldown_summary: false,
    }];
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);

    assert!(
        payloads.is_empty(),
        "TransactionFailed must never trigger on successful transactions"
    );
}

#[test]
fn test_attack_contract_upgraded_reports_wasm_hashes() {
    // An executable change must fire ContractUpgraded with old/new hashes.
    let raw = dummy_tx("upgrade_tx", true, None);
    let mut enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
    enriched.watched_contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
    enriched.wasm_upgrade = Some(txwatch_rules::WasmUpgrade {
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        old_wasm_hash: Some(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        ),
        new_wasm_hash: Some(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        ),
    });

    let rules = vec![RuleConfig {
        rule: AlertRule::ContractUpgraded,
        cooldown_seconds: None,
        cooldown_summary: false,
    }];
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert_eq!(payloads.len(), 1, "ContractUpgraded must fire on upgrade");
    let payload = &payloads[0];
    assert_eq!(
        payload.old_wasm_hash.as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(
        payload.new_wasm_hash.as_deref(),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    );

    // No upgrade recorded: rule must stay silent.
    enriched.wasm_upgrade = None;
    let payloads = evaluate(&test_ctx(), &rules, &enriched, None);
    assert!(
        payloads.is_empty(),
        "ContractUpgraded must not fire without an executable change"
    );
}
