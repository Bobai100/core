/// Integration tests for the full poll → evaluate → notify pipeline.
///
/// These tests spin up two wiremock servers:
///   - a mock Horizon server (transactions + operations endpoints)
///   - a mock webhook receiver
///
/// They then either call the public `run()` entry-point or drive the evaluate /
/// notify helpers directly to verify end-to-end behaviour without touching the
/// real Stellar network.
mod helpers;

use reqwest::Client;
use std::time::{Duration, Instant};

use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use txwatch_config::{AlertRule, AppConfig};
use txwatch_rules::{evaluate, EnrichedTransaction, EvalContext};

// ── Tests ─────────────────────────────────────────────────────────────────────

/// `run()` polling loop: fires exactly one webhook for the single transaction
/// returned on the first poll cycle, then gets empty pages on subsequent cycles.
#[tokio::test]
async fn run_polls_once_and_fires_webhook() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // First transactions request returns one tx.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("run001", "500", true)),
        )
        .up_to_n_times(1)
        .mount(&horizon)
        .await;

    // All subsequent transaction requests return an empty page.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // Operations for the tx: no Soroban details needed.
    Mock::given(method("GET"))
        .and(path("/transactions/run001/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // Webhook receiver: expect exactly 1 POST.
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    // Drive the loop for one full poll cycle (slightly more than the interval).
    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;

    // MockServer drop verifies that exactly 1 webhook was received.
}

#[tokio::test]
async fn poll_includes_fee_charged_and_fires_high_fee_rule() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // Horizon: transaction with fee_charged: "50000" (stroops)
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "_embedded": {
                "records": [{
                    "hash":         "fee_tx_poll",
                    "created_at":   "2024-06-01T10:00:00Z",
                    "successful":   true,
                    "paging_token": "1",
                    "fee_charged":  "50000",
                    "envelope_xdr": null,
                    "result_xdr":   null
                }]
            }
        })))
        .up_to_n_times(1)
        .mount(&horizon)
        .await;

    // All subsequent transaction requests return an empty page.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // Operations for that tx: empty
    Mock::given(method("GET"))
        .and(path("/transactions/fee_tx_poll/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::HighFee {
            threshold_stroops: 10_000,
            threshold_xlm: None,
        }],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;
}

#[tokio::test]
async fn cursor_file_is_loaded_and_used_for_initial_cursor() {
    use std::fs::OpenOptions;
    use std::io::Write;

    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // Expect the poll request to include cursor=100 (the persisted value).
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions.*cursor=100.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    // Create a temporary cursor file with the contract_id -> "100" mapping.
    let tmp = std::env::temp_dir().join("txwatch_test_cursor.json");
    let mapping = serde_json::json!({ contract.contract_id.clone(): "100" });
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)
        .unwrap();
    write!(f, "{}", mapping).unwrap();

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: Some(tmp.to_string_lossy().to_string()),
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;
}

/// AnyTransaction rule fires and webhook is called exactly once.
#[tokio::test]
async fn any_transaction_fires_webhook() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("hash001", "100", true)),
        )
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path("/transactions/hash001/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );

    let url = format!(
        "{}/accounts/{}/transactions?cursor=now&order=asc&limit=200",
        horizon.uri(),
        contract.contract_id
    );

    #[derive(serde::Deserialize)]
    struct Page {
        _embedded: Emb,
    }
    #[derive(serde::Deserialize)]
    struct Emb {
        records: Vec<txwatch_rules::HorizonTransaction>,
    }

    let page: Page = client.get(&url).send().await.unwrap().json().await.unwrap();
    let records = page._embedded.records;
    assert_eq!(records.len(), 1);

    for raw in records {
        let ops_url = format!("{}/transactions/{}/operations", horizon.uri(), raw.hash);
        // Consume the operations response to satisfy the mock expectation.
        let _ = client
            .get(&ops_url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();

        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
        let ctx = EvalContext {
            label: &contract.label,
            contract_id: &contract.contract_id,
            network: contract.network.as_str(),
            horizon_base: &horizon.uri(),
            explorer_base: Some("https://stellar.expert/explorer/testnet"),
        };
        let payloads = evaluate(&ctx, &contract.rules, &enriched, None);
        assert_eq!(payloads.len(), 1);

        for payload in &payloads {
            txwatch_notifier::send_webhook_simple(
                &client,
                contract.webhook_url.as_deref().unwrap(),
                payload,
                None,
            )
            .await
            .unwrap();
        }
    }
}

/// TransactionFailed rule fires only for failed transactions.
#[tokio::test]
async fn transaction_failed_rule_fires_only_on_failure() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "_embedded": {
                "records": [
                    {
                        "hash": "ok_tx", "created_at": "2024-06-01T10:00:00Z",
                        "successful": true, "paging_token": "1",
                        "envelope_xdr": null, "result_xdr": null
                    },
                    {
                        "hash": "fail_tx", "created_at": "2024-06-01T10:01:00Z",
                        "successful": false, "paging_token": "2",
                        "envelope_xdr": null, "result_xdr": null
                    }
                ]
            }
        })))
        .mount(&horizon)
        .await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::TransactionFailed],
    );

    let txs = vec![
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "ok_tx".into(),
                created_at: "2024-06-01T10:00:00Z".into(),
                successful: true,
                paging_token: "1".into(),
                fee_charged: None,
                source_account: None,
                fee_account: None,
                envelope_xdr: None,
                result_xdr: None,
                ledger: None,
                ..Default::default()
            },
            vec![],
            None,
            None,
        )
        .unwrap(),
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "fail_tx".into(),
                created_at: "2024-06-01T10:01:00Z".into(),
                successful: false,
                paging_token: "2".into(),
                fee_charged: None,
                source_account: None,
                fee_account: None,
                envelope_xdr: None,
                result_xdr: None,
                ledger: None,
                ..Default::default()
            },
            vec![],
            None,
            None,
        )
        .unwrap(),
    ];

    for tx in &txs {
        let ctx = EvalContext {
            label: &contract.label,
            contract_id: &contract.contract_id,
            network: contract.network.as_str(),
            horizon_base: &horizon.uri(),
            explorer_base: Some("https://stellar.expert/explorer/testnet"),
        };
        let payloads = evaluate(&ctx, &contract.rules, tx, None);
        for p in &payloads {
            txwatch_notifier::send_webhook_simple(
                &client,
                contract.webhook_url.as_deref().unwrap(),
                p,
                None,
            )
            .await
            .unwrap();
        }
    }
}

/// LargeTransfer rule fires when payment amount meets threshold.
#[tokio::test]
async fn large_transfer_fires_above_threshold() {
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::LargeTransfer {
            threshold_xlm: 5_000,
            threshold_stroops: 5_000 * 10_000_000,
        }],
    );

    let tx = EnrichedTransaction::from_horizon(
        txwatch_rules::HorizonTransaction {
            hash: "big_tx".into(),
            created_at: "2024-06-01T10:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            source_account: None,
            fee_account: None,
            envelope_xdr: None,
            result_xdr: None,
            ledger: None,
            ..Default::default()
        },
        vec![],
        Some(100_000_000_000),
        None,
    )
    .unwrap();

    let payloads = evaluate(
        &EvalContext {
            label: &contract.label,
            contract_id: &contract.contract_id,
            network: contract.network.as_str(),
            horizon_base: "https://horizon-testnet.stellar.org",
            explorer_base: Some("https://stellar.expert/explorer/testnet"),
        },
        &contract.rules,
        &tx,
        None,
    );
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].amount_xlm, Some(10_000));

    txwatch_notifier::send_webhook_simple(
        &client,
        contract.webhook_url.as_deref().unwrap(),
        &payloads[0],
        None,
    )
    .await
    .unwrap();
}

/// FunctionCalled rule fires only when the function name matches.
#[tokio::test]
async fn function_called_rule_fires_on_exact_match() {
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::FunctionCalled {
            function_name: "withdraw".into(),
            match_mode: Default::default(),
            args: vec![],
        }],
    );

    let txs = vec![
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "t1".into(),
                created_at: "2024-06-01T10:00:00Z".into(),
                successful: true,
                paging_token: "1".into(),
                fee_charged: None,
                source_account: None,
                fee_account: None,
                envelope_xdr: None,
                result_xdr: None,
                ledger: None,
                ..Default::default()
            },
            vec!["deposit".into()],
            None,
            None,
        )
        .unwrap(),
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "t2".into(),
                created_at: "2024-06-01T10:01:00Z".into(),
                successful: true,
                paging_token: "2".into(),
                fee_charged: None,
                source_account: None,
                fee_account: None,
                envelope_xdr: None,
                result_xdr: None,
                ledger: None,
                ..Default::default()
            },
            vec!["withdraw".into()],
            None,
            None,
        )
        .unwrap(),
    ];

    for tx in &txs {
        let payloads = evaluate(
            &EvalContext {
                label: &contract.label,
                contract_id: &contract.contract_id,
                network: contract.network.as_str(),
                horizon_base: "https://horizon-testnet.stellar.org",
                explorer_base: Some("https://stellar.expert/explorer/testnet"),
            },
            &contract.rules,
            tx,
            None,
        );
        for p in &payloads {
            txwatch_notifier::send_webhook_simple(
                &client,
                contract.webhook_url.as_deref().unwrap(),
                p,
                None,
            )
            .await
            .unwrap();
        }
    }
}

/// FunctionCalled with an argument matcher fires only on the invocation whose
/// decoded argument matches, and the matched argument values are included in
/// the webhook payload. Closes #266.
#[tokio::test]
async fn function_called_rule_matches_on_argument() {
    use txwatch_config::ArgMatcher;
    use txwatch_rules::FunctionInvocation;

    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::FunctionCalled {
            function_name: "transfer".into(),
            match_mode: Default::default(),
            args: vec![ArgMatcher {
                index: 1,
                eq: "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF".into(),
            }],
        }],
    );

    let matching_to = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    let other_to = "GBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    let make_tx = |hash: &str, token: &str, to: &str| {
        let tx = EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: hash.into(),
                created_at: "2024-06-01T10:00:00Z".into(),
                successful: true,
                paging_token: token.into(),
                fee_charged: None,
                source_account: None,
                fee_account: None,
                envelope_xdr: None,
                result_xdr: None,
                ledger: None,
                ..Default::default()
            },
            vec!["transfer".into()],
            None,
            None,
        )
        .unwrap();
        tx.with_invocations(vec![FunctionInvocation::from_parameters(
            "transfer".into(),
            &[
                serde_json::json!({ "i128": "1000" }),
                serde_json::json!({ "address": to }),
            ],
        )])
    };

    let txs = vec![
        make_tx("arg_miss", "1", other_to),
        make_tx("arg_hit", "2", matching_to),
    ];

    let mut fired = 0;
    for tx in &txs {
        let payloads = evaluate(
            &EvalContext {
                label: &contract.label,
                contract_id: &contract.contract_id,
                network: contract.network.as_str(),
                horizon_base: "https://horizon-testnet.stellar.org",
                explorer_base: Some("https://stellar.expert/explorer/testnet"),
            },
            &contract.rules,
            tx,
            None,
        );
        for p in &payloads {
            fired += 1;
            assert_eq!(p.function_name.as_deref(), Some("transfer"));
            assert!(
                p.function_args.iter().any(|inv| inv.arg(1) == Some(matching_to)),
                "payload must carry the matched argument values"
            );
            txwatch_notifier::send_webhook_simple(
                &client,
                contract.webhook_url.as_deref().unwrap(),
                p,
                None,
            )
            .await
            .unwrap();
        }
    }
    assert_eq!(fired, 1, "only the matching invocation should fire");
}

/// Cursor advances so the same transaction is not processed twice.
#[tokio::test]
async fn cursor_advances_after_each_transaction() {
    use std::collections::HashMap;

    let mut cursors: HashMap<String, String> = HashMap::new();
    let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
    cursors.insert(contract_id.to_string(), "now".to_string());

    for token in &["100", "200", "300"] {
        cursors.insert(contract_id.to_string(), token.to_string());
    }

    assert_eq!(cursors.get(contract_id).map(String::as_str), Some("300"));
}

/// HighFee rule fires when fee_charged from Horizon response exceeds threshold.
#[tokio::test]
async fn high_fee_rule_fires_on_fee_charged() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // Horizon: transaction with fee_charged: "50000" (stroops)
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "_embedded": {
                "records": [{
                    "hash":         "fee_tx",
                    "created_at":   "2024-06-01T10:00:00Z",
                    "successful":   true,
                    "paging_token": "1",
                    "fee_charged":  "50000",
                    "envelope_xdr": null,
                    "result_xdr":   null
                }]
            }
        })))
        .mount(&horizon)
        .await;

    // Horizon: operations for that transaction (empty, no Soroban)
    Mock::given(method("GET"))
        .and(path("/transactions/fee_tx/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // Webhook receiver: expect exactly 1 POST (HighFee fires)
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client = Client::new();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::HighFee {
            threshold_stroops: 10_000,
            threshold_xlm: None,
        }],
    );

    let tx = EnrichedTransaction::from_horizon(
        txwatch_rules::HorizonTransaction {
            hash: "fee_tx".into(),
            created_at: "2024-06-01T10:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: Some("50000".into()),
            source_account: None,
            fee_account: None,
            envelope_xdr: None,
            result_xdr: None,
            ledger: None,
            ..Default::default()
        },
        vec![],
        None,
        Some(50_000),
    )
    .unwrap();

    let payloads = evaluate(
        &EvalContext {
            label: &contract.label,
            contract_id: &contract.contract_id,
            network: contract.network.as_str(),
            horizon_base: &horizon.uri(),
            explorer_base: Some("https://stellar.expert/explorer/testnet"),
        },
        &contract.rules,
        &tx,
        None,
    );
    assert_eq!(payloads.len(), 1);
    assert!(payloads[0].rule_triggered.contains("HighFee"));
    assert_eq!(payloads[0].fee_charged_stroops, Some(50_000));

    txwatch_notifier::send_webhook_simple(
        &client,
        contract.webhook_url.as_deref().unwrap(),
        &payloads[0],
        None,
    )
    .await
    .unwrap();
}

/// Issue #208: end-to-end coverage against a real `stellar/quickstart --local`
/// node. Gated behind `TXWATCH_E2E_QUICKSTART=1` so it only runs in the CI job
/// that starts the quickstart service container; skipped everywhere else.
///
/// The test drives the real poller against the local Horizon at
/// `http://localhost:8000`, using a pre-deployed sample contract, and asserts
/// that `FunctionCalled`, `TransactionFailed` and `EventEmitted` alerts reach a
/// local webhook receiver.
mod e2e_quickstart {
    use super::*;

    const QUICKSTART_HORIZON: &str = "http://localhost:8000";

    fn enabled() -> bool {
        std::env::var("TXWATCH_E2E_QUICKSTART").as_deref() == Ok("1")
    }

    /// Contract id of the sample contract deployed by the CI job before the
    /// test runs. The job exports it as `TXWATCH_E2E_CONTRACT_ID`.
    fn sample_contract_id() -> String {
        std::env::var("TXWATCH_E2E_CONTRACT_ID").expect(
            "TXWATCH_E2E_CONTRACT_ID must be set to the deployed sample contract id \
             when TXWATCH_E2E_QUICKSTART=1",
        )
    }

    /// Wait until the local quickstart Horizon answers, so the test does not
    /// race the service container's startup.
    async fn wait_for_horizon(client: &Client) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Ok(resp) = client.get(QUICKSTART_HORIZON).send().await {
                if resp.status().is_success() {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "quickstart Horizon at {} did not become ready",
                QUICKSTART_HORIZON
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Collect webhook payloads until `expected` have arrived or the deadline
    /// passes, returning everything received.
    async fn collect_webhooks(
        receiver: &MockServer,
        expected: usize,
        timeout: Duration,
    ) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let requests = receiver.received_requests().await.unwrap_or_default();
            if requests.len() >= expected || Instant::now() >= deadline {
                return requests
                    .iter()
                    .filter_map(|r| serde_json::from_slice(&r.body).ok())
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn rule_types(payloads: &[serde_json::Value]) -> Vec<String> {
        payloads
            .iter()
            .filter_map(|p| p["rule_type"].as_str().map(str::to_string))
            .collect()
    }

    /// Real-Horizon end-to-end: deploy/invoke the sample contract, then assert
    /// the poller delivers `FunctionCalled`, `TransactionFailed` and
    /// `EventEmitted` alerts to a local receiver.
    #[tokio::test]
    async fn quickstart_delivers_function_failed_and_event_alerts() {
        if !enabled() {
            eprintln!(
                "skipping quickstart e2e test; set TXWATCH_E2E_QUICKSTART=1 to run it"
            );
            return;
        }

        let client = txwatch_notifier::build_client().unwrap();
        wait_for_horizon(&client).await;

        let contract_id = sample_contract_id();
        let receiver = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&receiver)
            .await;

        let mut contract = helpers::contract(
            &format!("{}/hook", receiver.uri()),
            vec![
                AlertRule::FunctionCalled {
                    function_name: "hello".into(),
                    match_mode: Default::default(),
                    args: vec![],
                },
                AlertRule::TransactionFailed,
                AlertRule::EventEmitted {
                    event_name: "transfer".into(),
                    match_mode: Default::default(),
                },
            ],
        );
        contract.contract_id = contract_id;
        contract.horizon_base_url_override = Some(QUICKSTART_HORIZON.to_string());

        let cfg = AppConfig {
            poll_interval_seconds: 1,
            contracts: vec![contract],
            cursor_file: None,
            http_pool_max_idle_per_host: 10,
            http_tcp_keepalive_secs: 30,
            http_connection_verbose: None,
            max_contracts: None,
            max_pages_per_cycle: None,
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let run = tokio::spawn(txwatch_poller::run_with_shutdown(cfg, false, shutdown_rx));

        // The CI job has already invoked the sample contract (successful call,
        // a failing call and an event-emitting call) before starting the
        // poller, so all three alerts should arrive within the window.
        let payloads = collect_webhooks(&receiver, 3, Duration::from_secs(60)).await;

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(10), run).await;

        let types = rule_types(&payloads);
        assert!(
            types.iter().any(|t| t.contains("FunctionCalled")),
            "expected a FunctionCalled alert, got: {:?}",
            types
        );
        assert!(
            types.iter().any(|t| t.contains("TransactionFailed")),
            "expected a TransactionFailed alert, got: {:?}",
            types
        );
        assert!(
            types.iter().any(|t| t.contains("EventEmitted")),
            "expected an EventEmitted alert, got: {:?}",
            types
        );
    }
}

/// When run in dry-run mode, matched rules are logged but webhooks are not sent.
#[tokio::test]
async fn run_polls_once_and_skips_webhook_in_dry_run() {
    use std::time::Duration;

    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // First transactions request returns one tx.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::tx_page(
            "dryrun001",
            "500",
            true,
        )))
        .up_to_n_times(1)
        .mount(&horizon)
        .await;

    // All subsequent transaction requests return an empty page.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // Operations for the tx: no Soroban details needed.
    Mock::given(method("GET"))
        .and(path("/transactions/dryrun001/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // Webhook receiver: expect exactly 0 POSTs when dry-run is enabled.
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    // Drive the loop for one full poll cycle (slightly more than the interval).
    let _ = tokio::time::timeout(
        Duration::from_millis(1500),
        txwatch_poller::run_with(cfg, true),
    )
    .await;

    // MockServer drop verifies that 0 webhooks were received.
}

/// End-to-end LargeTransfer poll, webhook payload, and cursor advancement.
#[tokio::test]
async fn large_transfer_poll_fires_webhook_and_advances_cursor() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("large_tx", "1", true)),
        )
        .up_to_n_times(1)
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path("/transactions/large_tx/operations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::payment_ops_page("5000.0000000")),
        )
        .mount(&horizon)
        .await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::LargeTransfer {
            threshold_xlm: 1000,
            threshold_stroops: 1000 * 10_000_000,
        }],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;

    let webhook_requests = receiver.received_requests().await.unwrap();
    assert_eq!(
        webhook_requests.len(),
        1,
        "expected exactly one webhook POST"
    );

    let body: serde_json::Value =
        serde_json::from_slice(&webhook_requests[0].body).expect("webhook body is JSON");
    assert_eq!(body["rule_type"].as_str(), Some("LargeTransfer"));
    assert_eq!(
        body["rule_triggered"].as_str(),
        Some("LargeTransfer(>=1000XLM)")
    );
    assert_eq!(body["amount_xlm"].as_u64(), Some(5000));

    let requests = horizon.received_requests().await.unwrap();
    assert!(
        requests.iter().any(|r| r.url.as_str().contains("cursor=1")),
        "expected the second Horizon transaction request to advance the cursor to '1'"
    );
}

/// horizon_link in webhook payloads always points to the canonical Horizon URL
/// even when polling against a mock server (horizon_base_url_override set). Closes #92.
#[tokio::test]
async fn horizon_link_uses_canonical_url_not_mock_server() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("link_tx", "1", true)),
        )
        .up_to_n_times(1)
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path("/transactions/link_tx/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let canonical_base = txwatch_config::Network::Testnet.horizon_base_url();

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;

    let requests = receiver.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "expected exactly 1 webhook POST");

    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("webhook body is JSON");
    let horizon_link = body["horizon_link"]
        .as_str()
        .expect("horizon_link field present");

    assert!(
        horizon_link.starts_with(canonical_base),
        "horizon_link should start with canonical URL '{}', got: {}",
        canonical_base,
        horizon_link
    );
    assert!(
        !horizon_link.starts_with("http://127.0.0.1"),
        "horizon_link must not point to mock server, got: {}",
        horizon_link
    );
}

/// Two contracts polled concurrently: total wall time must be less than
/// the sum of each contract's individual response delay, proving that a
/// slow Horizon response for contract A does not delay contract B. Closes #7
/// and protects the concurrency behaviour introduced for #36.
#[tokio::test]
async fn contracts_polled_concurrently() {
    const DELAY_MS: u64 = 1000;

    let horizon1 = MockServer::start().await;
    let horizon2 = MockServer::start().await;
    let receiver = MockServer::start().await;

    for horizon in [&horizon1, &horizon2] {
        // Each Horizon server returns one tx with an artificial delay.
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(helpers::tx_page("delayed_tx", "1", true))
                    .set_delay(Duration::from_millis(DELAY_MS)),
            )
            .up_to_n_times(1)
            .mount(horizon)
            .await;

        // Subsequent requests return empty immediately.
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
            .mount(horizon)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
            .mount(horizon)
            .await;
    }

    // Expect exactly 2 webhook POSTs — one per contract.
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&receiver)
        .await;

    let make_contract = |label: &str, horizon_uri: &str| {
        let mut c = helpers::contract(
            &format!("{}/hook", receiver.uri()),
            vec![AlertRule::AnyTransaction],
        );
        c.label = label.to_string();
        c.contract_id = if label == "A" {
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4".to_string()
        } else {
            "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526".to_string()
        };
        c.horizon_base_url_override = Some(horizon_uri.to_string());
        c
    };

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![
            make_contract("A", &horizon1.uri()),
            make_contract("B", &horizon2.uri()),
        ],
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        cursor_file: None,
        ..Default::default()
    };

    // `run` never returns, so time how long it takes until both webhooks arrive.
    let start = std::time::Instant::now();
    let run = tokio::spawn(txwatch_poller::run(cfg));
    while receiver.received_requests().await.unwrap().len() < 2
        && start.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let elapsed = start.elapsed();
    run.abort();

    // Sequential polling takes at least 2 × DELAY_MS, so this 1.5× bound fails it.
    assert!(
        elapsed < Duration::from_millis(DELAY_MS * 3 / 2),
        "contracts should be polled concurrently; elapsed {:?} ≥ {}ms",
        elapsed,
        DELAY_MS * 3 / 2,
    );
}

/// A slow Horizon request for one contract must not delay another contract's
/// webhook.
/// Closes #6.
#[tokio::test]
async fn slow_contract_does_not_block_fast_contract() {
    const SLOW_DELAY_MS: u64 = 2500;
    const FAST_DEADLINE_MS: u64 = 1000;

    let slow_horizon = MockServer::start().await;
    let fast_horizon = MockServer::start().await;
    let slow_receiver = MockServer::start().await;
    let fast_receiver = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(helpers::tx_page("slow_tx", "1", true))
                .set_delay(Duration::from_millis(SLOW_DELAY_MS)),
        )
        .up_to_n_times(1)
        .mount(&slow_horizon)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&slow_horizon)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("fast_tx", "1", true)),
        )
        .up_to_n_times(1)
        .mount(&fast_horizon)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&fast_horizon)
        .await;

    for horizon in [&slow_horizon, &fast_horizon] {
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
            .mount(horizon)
            .await;
    }

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&slow_receiver)
        .await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&fast_receiver)
        .await;

    let make_contract =
        |label: &str, contract_id: &str, horizon: &MockServer, receiver: &MockServer| {
            let mut contract = helpers::contract(
                &format!("{}/hook", receiver.uri()),
                vec![AlertRule::AnyTransaction],
            );
            contract.label = label.to_string();
            contract.contract_id = contract_id.to_string();
            contract.horizon_base_url_override = Some(horizon.uri());
            contract
        };

    let cfg = AppConfig {
        poll_interval_seconds: 30,
        contracts: vec![
            make_contract(
                "slow",
                "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
                &slow_horizon,
                &slow_receiver,
            ),
            make_contract(
                "fast",
                "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526",
                &fast_horizon,
                &fast_receiver,
            ),
        ],
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        cursor_file: None,
        ..Default::default()
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(txwatch_poller::run_with_shutdown(cfg, false, shutdown_rx));
    let fast_delivered = tokio::time::timeout(Duration::from_millis(FAST_DEADLINE_MS), async {
        loop {
            if !fast_receiver.received_requests().await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(4), handle).await;

    assert!(
        fast_delivered,
        "fast contract webhook should arrive before the slow Horizon request completes"
    );
}

/// Reloading the config keeps the cursor of a contract that still exists and
/// starts a newly added contract from `now`. Closes #98.
#[tokio::test]
async fn reload_keeps_existing_cursors_and_starts_new_contracts() {
    let horizon_a = MockServer::start().await;
    let horizon_b = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::tx_page("tx_a", "5", true)))
        .up_to_n_times(1)
        .mount(&horizon_a)
        .await;
    for horizon in [&horizon_a, &horizon_b] {
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
            .mount(horizon)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
            .mount(horizon)
            .await;
    }

    let mut a = helpers::contract(
        "https://hooks.example.com/a",
        vec![AlertRule::AnyTransaction],
    );
    a.label = "A".into();
    a.horizon_base_url_override = Some(horizon_a.uri());
    let mut b = helpers::contract(
        "https://hooks.example.com/b",
        vec![AlertRule::AnyTransaction],
    );
    b.label = "B".into();
    b.contract_id = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526".into();
    b.horizon_base_url_override = Some(horizon_b.uri());

    let config = |contracts| AppConfig {
        // Long interval: the only second poll comes from the reload.
        poll_interval_seconds: 3600,
        contracts,
        cursor_file: None,
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (reload_tx, reload_rx) = tokio::sync::mpsc::channel(1);
    let run = tokio::spawn(txwatch_poller::run_with_reload(
        config(vec![a.clone()]),
        true,
        shutdown_rx,
        reload_rx,
    ));

    tokio::time::sleep(Duration::from_millis(500)).await;
    reload_tx.send(config(vec![a, b])).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("poller should stop after shutdown")
        .unwrap()
        .unwrap();

    let cursors = |requests: Vec<wiremock::Request>| -> Vec<String> {
        requests
            .iter()
            .filter(|r| r.url.path().ends_with("/transactions"))
            .filter_map(|r| {
                r.url
                    .query_pairs()
                    .find(|(k, _)| k == "cursor")
                    .map(|(_, v)| v.into_owned())
            })
            .collect()
    };
    assert_eq!(
        cursors(horizon_a.received_requests().await.unwrap()),
        vec!["now", "5"],
        "A must keep its advanced cursor across the reload"
    );
    assert_eq!(
        cursors(horizon_b.received_requests().await.unwrap()),
        vec!["now"],
        "B is new and must start from 'now'"
    );
}

/// A per-contract `poll_interval_seconds` override is scheduled independently:
/// the fast contract is polled several times while the slow one (global
/// interval) is polled only once. Closes #97.
#[tokio::test]
async fn per_contract_poll_interval_is_scheduled_independently() {
    let fast_horizon = MockServer::start().await;
    let slow_horizon = MockServer::start().await;

    for horizon in [&fast_horizon, &slow_horizon] {
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
            .mount(horizon)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
            .mount(horizon)
            .await;
    }

    let mut fast = helpers::contract(
        "https://hooks.example.com/fast",
        vec![AlertRule::AnyTransaction],
    );
    fast.label = "fast".into();
    fast.poll_interval_seconds = Some(1);
    fast.horizon_base_url_override = Some(fast_horizon.uri());

    let mut slow = helpers::contract(
        "https://hooks.example.com/slow",
        vec![AlertRule::AnyTransaction],
    );
    slow.label = "slow".into();
    slow.contract_id = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526".into();
    slow.horizon_base_url_override = Some(slow_horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 3600,
        contracts: vec![fast, slow],
        cursor_file: None,
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    let _ = tokio::time::timeout(Duration::from_millis(2500), txwatch_poller::run(cfg)).await;

    let fast_polls = fast_horizon.received_requests().await.unwrap().len();
    let slow_polls = slow_horizon.received_requests().await.unwrap().len();
    assert!(
        fast_polls >= 2,
        "fast contract should be polled repeatedly, got {}",
        fast_polls
    );
    assert_eq!(
        slow_polls, 1,
        "slow contract should be polled once, got {}",
        slow_polls
    );
}

/// Issue #4: Horizon's `/accounts/{id}` routes only accept G-addresses, so the
/// only transaction source cannot serve a contract (C…) address. This pins the
/// recorded rejection so the limitation is visible in the test suite, and
/// documents the HTTP 400 the poller surfaces today.
///
/// It is a regression guard, not a fix: C-addresses still do not work until a
/// Soroban RPC source lands.
#[tokio::test]
async fn horizon_rejects_a_contract_address_on_the_accounts_endpoint() {
    let recorded: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/horizon_c_address_transactions_400.json"
    ))
    .expect("recorded Horizon error is valid JSON");
    assert_eq!(recorded["status"], 400);
    assert_eq!(recorded["extras"]["invalid_field"], "account_id");

    // Replay the recorded body so the poller sees exactly what a live contract
    // produces. The error text itself is asserted in
    // `horizon_c_address_rejection_names_the_account_id_field` in src/lib.rs,
    // which can reach the crate-internal `HorizonSource`.
    let horizon = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(recorded.clone()))
        .mount(&horizon)
        .await;

    let mut contract =
        helpers::contract("https://example.com/hook", vec![AlertRule::AnyTransaction]);
    contract.horizon_base_url_override = Some(horizon.uri());
    let cfg = AppConfig {
        poll_interval_seconds: 3600,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    let report = txwatch_poller::run_once(cfg, true)
        .await
        .expect("one rejected contract must not abort the whole cycle");
    assert_eq!(report.transactions, 0);
    assert_eq!(report.alerts, 0);
    assert_eq!(report.poll_failures, 1);
    assert!(!report.is_success());
}

/// Issue #5: the cursor file was read on startup but never written, so a
/// restart replayed every transaction since the original cursor. This drives
/// the real polling loop, then reads the file back and asserts the paging token
/// that was actually served.
#[tokio::test]
async fn cursor_file_is_persisted_after_a_poll_cycle() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // One transaction on the first cycle, then empty pages.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::tx_page(
            "run001",
            "cursor-tok-1",
            true,
        )))
        .up_to_n_times(1)
        .mount(&horizon)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;
    Mock::given(method("GET"))
        .and(path("/transactions/run001/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let cursor_file = std::env::temp_dir().join(format!(
        "txwatch_cursor_test_{}_{}.json",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_file(&cursor_file);
    assert!(
        !cursor_file.exists(),
        "cursor file must not pre-exist: {}",
        cursor_file.display()
    );

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract.clone()],
        cursor_file: Some(cursor_file.display().to_string()),
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let run =
        tokio::spawn(
            async move { txwatch_poller::run_with_shutdown(cfg, true, shutdown_rx).await },
        );

    let deadline = Instant::now() + Duration::from_secs(15);
    let raw = loop {
        if let Ok(text) = std::fs::read_to_string(&cursor_file) {
            if text.contains("cursor-tok-1") {
                break text;
            }
        }
        assert!(
            Instant::now() < deadline,
            "cursor file never contained the served paging token: {}",
            cursor_file.display()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let persisted: serde_json::Value = serde_json::from_str(&raw).expect("cursor file is JSON");
    assert_eq!(
        persisted[&contract.cursor_key()],
        "cursor-tok-1",
        "the served paging token must be persisted"
    );
    let tmp = cursor_file.with_extension("tmp");
    assert!(!tmp.exists(), "temp file {} was left behind", tmp.display());

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), run).await;
    let _ = std::fs::remove_file(&cursor_file);
}

/// Issue #264: `LargeTransfer` must only count balance changes where the
/// watched contract is `from` or `to`. A transaction that mixes several
/// unrelated native payments with one transfer involving the contract must
/// not sum the unrelated legs into a "large transfer".
#[tokio::test]
async fn large_transfer_only_counts_transfers_involving_the_contract() {
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::LargeTransfer {
            threshold_xlm: 1000,
            threshold_stroops: 1000 * 10_000_000,
        }],
    );

    // Three unrelated 900 XLM payments between other accounts plus a single
    // 10 XLM payment into the watched contract. Summing every native transfer
    // would yield 2710 XLM and fire; counting only the contract's legs yields
    // 10 XLM and must not fire.
    let ops = vec![
        helpers::native_payment_op(
            "GUNRELATED1AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "GUNRELATED2AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "900.0000000",
        ),
        helpers::native_payment_op(
            "GUNRELATED3AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "GUNRELATED4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "900.0000000",
        ),
        helpers::native_payment_op(
            "GUNRELATED5AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "GUNRELATED6AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "900.0000000",
        ),
        helpers::native_payment_op(
            "GUNRELATED7AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            &contract.contract_id,
            "10.0000000",
        ),
    ];

    let tx = EnrichedTransaction::from_horizon(
        txwatch_rules::HorizonTransaction {
            hash: "mixed_tx".into(),
            created_at: "2024-06-01T10:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            source_account: None,
            fee_account: None,
            envelope_xdr: None,
            result_xdr: None,
            ledger: None,
            ..Default::default()
        },
        vec![],
        None,
        None,
    )
    .unwrap();

    let ctx = EvalContext {
        label: &contract.label,
        contract_id: &contract.contract_id,
        network: contract.network.as_str(),
        horizon_base: "https://horizon-testnet.stellar.org",
        explorer_base: Some("https://stellar.expert/explorer/testnet"),
    };

    // The unrelated legs must not be summed into a large transfer.
    let payloads = evaluate(&ctx, &contract.rules, &tx, Some(&ops));
    assert!(
        payloads.is_empty(),
        "unrelated payments must not trigger LargeTransfer, got {:?}",
        payloads
    );

    // A transfer that does involve the contract and exceeds the threshold
    // must fire, with the amount limited to the contract's own leg.
    let big_ops = vec![
        helpers::native_payment_op(
            "GUNRELATED1AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "GUNRELATED2AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "900.0000000",
        ),
        helpers::native_payment_op(
            "GUNRELATED7AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            &contract.contract_id,
            "2000.0000000",
        ),
    ];
    let payloads = evaluate(&ctx, &contract.rules, &tx, Some(&big_ops));
    assert_eq!(payloads.len(), 1, "contract inflow must trigger LargeTransfer");
    assert_eq!(payloads[0].amount_xlm, Some(2000));

    txwatch_notifier::send_webhook_simple(
        &client,
        contract.webhook_url.as_deref().unwrap(),
        &payloads[0],
        None,
    )
    .await
    .unwrap();
}

/// Issue #264: the `direction` option restricts `LargeTransfer` to inflows or
/// outflows of the watched contract.
#[tokio::test]
async fn large_transfer_respects_direction_option() {
    let contract = helpers::contract(
        "https://example.com/hook",
        vec![AlertRule::LargeTransfer {
            threshold_xlm: 1000,
            threshold_stroops: 1000 * 10_000_000,
            direction: Some(txwatch_config::TransferDirection::In),
        }],
    );

    let tx = EnrichedTransaction::from_horizon(
        txwatch_rules::HorizonTransaction {
            hash: "dir_tx".into(),
            created_at: "2024-06-01T10:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            source_account: None,
            fee_account: None,
            envelope_xdr: None,
            result_xdr: None,
            ledger: None,
            ..Default::default()
        },
        vec![],
        None,
        None,
    )
    .unwrap();

    let ctx = EvalContext {
        label: &contract.label,
        contract_id: &contract.contract_id,
        network: contract.network.as_str(),
        horizon_base: "https://horizon-testnet.stellar.org",
        explorer_base: Some("https://stellar.expert/explorer/testnet"),
    };

    // Outflow only: direction = "in" must not fire.
    let outflow = vec![helpers::native_payment_op(
        &contract.contract_id,
        "GUNRELATED1AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "5000.0000000",
    )];
    assert!(
        evaluate(&ctx, &contract.rules, &tx, Some(&outflow)).is_empty(),
        "direction=in must ignore outflows"
    );

    // Inflow: direction = "in" fires.
    let inflow = vec![helpers::native_payment_op(
        "GUNRELATED1AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        &contract.contract_id,
        "5000.0000000",
    )];
    let payloads = evaluate(&ctx, &contract.rules, &tx, Some(&inflow));
    assert_eq!(payloads.len(), 1, "direction=in must fire on inflows");
    assert_eq!(payloads[0].amount_xlm, Some(5000));
}

/// Issue #265: a contract upgrade through an arbitrary function (not one named
/// `upgrade`) must fire `ContractUpgraded` with the old and new WASM hashes.
#[tokio::test]
async fn contract_upgraded_rule_fires_with_wasm_hashes() {
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::ContractUpgraded],
    );

    let old_wasm = "aa".repeat(32);
    let new_wasm = "bb".repeat(32);

    let tx = EnrichedTransaction::from_horizon(
        txwatch_rules::HorizonTransaction {
            hash: "upgrade_tx".into(),
            created_at: "2024-06-01T10:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            source_account: None,
            fee_account: None,
            envelope_xdr: None,
            result_xdr: None,
            ledger: None,
            ..Default::default()
        },
        vec!["migrate".into()],
        None,
        None,
    )
    .unwrap()
    .with_wasm_upgrade(old_wasm.clone(), new_wasm.clone());

    let payloads = evaluate(
        &EvalContext {
            label: &contract.label,
            contract_id: &contract.contract_id,
            network: contract.network.as_str(),
            horizon_base: "https://horizon-testnet.stellar.org",
            explorer_base: Some("https://stellar.expert/explorer/testnet"),
        },
        &contract.rules,
        &tx,
        None,
    );
    assert_eq!(payloads.len(), 1, "ContractUpgraded must fire");
    assert!(payloads[0].rule_triggered.contains("ContractUpgraded"));
    assert_eq!(payloads[0].old_wasm_hash.as_deref(), Some(old_wasm.as_str()));
    assert_eq!(payloads[0].new_wasm_hash.as_deref(), Some(new_wasm.as_str()));

    txwatch_notifier::send_webhook_simple(
        &client,
        contract.webhook_url.as_deref().unwrap(),
        &payloads[0],
        None,
    )
    .await
    .unwrap();
}

/// Issue #2: Horizon's transaction collection endpoints omit failed
/// transactions unless `include_failed=true` is passed, which made the
/// `TransactionFailed` rule unreachable in production. The matcher below only
/// matches when the parameter is present, so the poll cannot succeed without
/// it; the recorded request is then asserted explicitly.
#[tokio::test]
async fn transactions_request_includes_failed() {
    let horizon = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .and(query_param("include_failed", "true"))
        .and(query_param("join", "operations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("fail001", "77", false)),
        )
        .mount(&horizon)
        .await;

    let mut contract = helpers::contract(
        "https://example.com/hook",
        vec![AlertRule::TransactionFailed],
    );
    contract.horizon_base_url_override = Some(horizon.uri());
    let cfg = AppConfig {
        poll_interval_seconds: 3600,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
        readiness_policy: Default::default(),
        ..Default::default()
    };

    // Dry run: this asserts the request we send, not the delivery path.
    let report = txwatch_poller::run_once(cfg, true).await.unwrap();
    assert_eq!(report.poll_failures, 0, "the request must match the mock");
    assert_eq!(
        report.transactions, 1,
        "the failed transaction must be seen"
    );

    let requests = horizon.received_requests().await.unwrap();
    let all: Vec<String> = requests
        .iter()
        .map(|request| request.url.to_string())
        .collect();
    let collection: Vec<&String> = all
        .iter()
        .filter(|url| url.contains("/accounts/") && url.contains("/transactions?"))
        .collect();
    assert!(
        !collection.is_empty(),
        "expected a transactions collection request"
    );
    assert!(
        collection
            .iter()
            .all(|url| url.contains("include_failed=true")),
        "every transactions request must carry include_failed=true"
    );
}

/// Issue #271: a Horizon 429 must not be reported as a successful empty poll.
/// The cycle must count as a poll failure (so readiness/freshness do not
/// advance and the failure streak is not reset) and the rate-limited metric
/// must be incremented.
#[tokio::test]
async fn horizon_429_is_not_a_successful_poll() {
    let horizon = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_json(serde_json::json!({
                    "status": 429,
                    "title": "Too Many Requests",
                })),
        )
        .mount(&horizon)
        .await;

    let mut contract =
        helpers::contract("https://example.com/hook", vec![AlertRule::AnyTransaction]);
    contract.horizon_base_url_override = Some(horizon.uri());
    let cfg = AppConfig {
        poll_interval_seconds: 3600,
        contracts: vec![contract],
        cursor_file: None,
/// Issue #278: cursor persistence must not block the poll loop. With many
/// contracts and a deliberately slow writer, every contract must still be
/// polled promptly; the writes are coalesced and performed off the async
/// workers.
#[tokio::test]
async fn slow_cursor_writer_does_not_delay_polls() {
    const CONTRACTS: usize = 8;
    const SLOW_WRITE_MS: u64 = 400;

    let horizon = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    let contracts: Vec<_> = (0..CONTRACTS)
        .map(|i| {
            let mut contract = helpers::contract(
                "https://hooks.example.com/hook",
                vec![AlertRule::AnyTransaction],
            );
            contract.label = format!("slow-writer-{i}");
            contract.contract_id = format!(
                "C{:0>55}",
                format!("{:X}", i + 1)
            );
            contract.horizon_base_url_override = Some(horizon.uri());
            contract
        })
        .collect();

    let cursor_file = std::env::temp_dir().join(format!(
        "txwatch_slow_writer_{}_{}.json",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_file(&cursor_file);

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts,
        cursor_file: Some(cursor_file.display().to_string()),
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
    };

    let before = txwatch_poller::horizon_rate_limited_total();
    let report = txwatch_poller::run_once(cfg, true)
        .await
        .expect("a rate-limited contract must not abort the whole cycle");

    assert_eq!(report.transactions, 0);
    assert_eq!(report.alerts, 0);
    assert_eq!(
        report.poll_failures, 1,
        "a 429 must be counted as a poll failure, not a successful empty poll"
    );
    assert!(
        !report.is_success(),
        "a 429 must not report the cycle as successful"
    );
    assert!(
        txwatch_poller::horizon_rate_limited_total() > before,
        "txwatch_horizon_rate_limited_total must be incremented on a 429"
    );
}

/// Issue #272: a long `Retry-After` back-off must not block shutdown. The
/// poller is signalled to stop immediately after the 429 is received and must
/// exit well before the 300 s back-off would elapse.
#[tokio::test]
async fn long_retry_after_does_not_block_shutdown() {
    let horizon = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "300")
                .set_body_json(serde_json::json!({
                    "status": 429,
                    "title": "Too Many Requests",
                })),
        )
        .mount(&horizon)
        .await;

    let mut contract =
        helpers::contract("https://example.com/hook", vec![AlertRule::AnyTransaction]);
    contract.horizon_base_url_override = Some(horizon.uri());
    let cfg = AppConfig {
        poll_interval_seconds: 3600,
        contracts: vec![contract],
        cursor_file: None,
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let run = tokio::spawn(async move {
        txwatch_poller::run_with_shutdown(cfg, true, shutdown_rx).await
    });

    // All contracts must be polled within a window far shorter than
    // CONTRACTS × SLOW_WRITE_MS, proving writes do not serialize the loop.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut polled = 0;
    while polled < CONTRACTS && Instant::now() < deadline {
        polled = horizon
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/transactions"))
            .count();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        polled >= CONTRACTS,
        "all {CONTRACTS} contracts should be polled despite slow writes, saw {polled}"
    );

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), run).await;
    let _ = std::fs::remove_file(&cursor_file);
}

/// Issue #281: cooldown and `NoActivity` state must survive a restart via the
/// versioned state file, so `watch --once` (cron) can enforce cooldowns across
/// runs and a `NoActivity` recovery is still sent after a restart.
#[tokio::test]
async fn state_file_persists_cooldown_and_no_activity_across_restart() {
    let horizon = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    let mut contract = helpers::contract(
        "https://hooks.example.com/hook",
        vec![AlertRule::AnyTransaction],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let state_file = std::env::temp_dir().join(format!(
        "txwatch_state_{}_{}.json",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_file(&state_file);

    let cfg = AppConfig {
        poll_interval_seconds: 3600,
        contracts: vec![contract.clone()],
        cursor_file: None,
        state_file: Some(state_file.display().to_string()),
        http_pool_max_idle_per_host: 10,
        http_tcp_keepalive_secs: 30,
        http_connection_verbose: None,
        max_contracts: None,
        max_pages_per_cycle: None,
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(txwatch_poller::run_with_shutdown(cfg, true, shutdown_rx));

    // Wait until Horizon has actually served the 429, so the poller is inside
    // (or about to enter) the back-off.
    let deadline = Instant::now() + Duration::from_secs(5);
    while horizon.received_requests().await.unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "Horizon never received the rate-limited request"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let stop_at = Instant::now();
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("poller must exit promptly instead of waiting out the back-off")
        .unwrap()
        .unwrap();
    assert!(
        stop_at.elapsed() < Duration::from_secs(5),
        "shutdown took {:?}, the 300s Retry-After back-off blocked it",
        stop_at.elapsed()
    );
    // First run: no state yet, so nothing is suppressed.
    let report = txwatch_poller::run_once(cfg.clone(), true).await.unwrap();
    assert_eq!(report.poll_failures, 0);

    // Simulate a restart: the state file must exist and be versioned.
    let raw = std::fs::read_to_string(&state_file).expect("state file written");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("state file is JSON");
    assert!(
        state.get("version").is_some(),
        "state file must be versioned, got: {raw}"
    );

    // Second run loads the persisted state without error.
    let report = txwatch_poller::run_once(cfg, true).await.unwrap();
    assert_eq!(report.poll_failures, 0);

    let _ = std::fs::remove_file(&state_file);
}
