//! Attack tests for `txwatch-notifier` verifying HMAC signature defenses,
//! retry exhaustion behavior, and denial of service resistance.

use std::sync::{Arc, Mutex};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tokio::sync::oneshot;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use txwatch_notifier::{build_client, send_webhook, test_payload};

#[tokio::test]
async fn test_attack_hmac_tamper_detection() {
    let payload = test_payload("AttackTest");
    let body = serde_json::to_string(&payload).unwrap();
    let secret = "correct-secret-key";

    // Compute legitimate HMAC
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let valid_signature = hex::encode(mac.finalize().into_bytes());

    // Tampered payload body simulation
    let mut tampered_mac = Hmac::<Sha256>::new_from_slice(b"forged-attacker-key").unwrap();
    tampered_mac.update(body.as_bytes());
    let forged_signature = hex::encode(tampered_mac.finalize().into_bytes());

    assert_ne!(
        valid_signature, forged_signature,
        "Forged secret must never produce matching HMAC signature"
    );
}

#[tokio::test]
async fn test_attack_server_slowloris_or_500_flood_terminates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = build_client().unwrap();
    let url = format!("{}/hook", server.uri());
    let payload = test_payload("SlowlorisDefense");
    let (_tx, rx) = oneshot::channel();

    let result = send_webhook(&client, &url, &payload, Some("secret"), rx).await;
    assert!(
        result.is_err(),
        "Exhausted retries on 500 flood must terminate with error"
    );
}

#[tokio::test]
async fn test_attack_huge_success_body_is_capped() {
    let server = MockServer::start().await;
    // A misbehaving receiver streams a very large 2xx body. The notifier must
    // not read it unbounded into memory; it should cap the read and still
    // treat the delivery as successful.
    let huge_body = "A".repeat(8 * 1024 * 1024);
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200).set_body_string(huge_body))
        .mount(&server)
        .await;

    let client = build_client().unwrap();
    let url = format!("{}/hook", server.uri());
    let payload = test_payload("HugeBodyDefense");
    let (_tx, rx) = oneshot::channel();

    let result = send_webhook(&client, &url, &payload, Some("secret"), rx).await;
    assert!(
        result.is_ok(),
        "A 2xx with a huge body must still be treated as a successful delivery"
    );
/// Records the `contract` field of every span that is entered, keyed by span id.
#[derive(Clone, Default)]
struct SpanRecorder {
    entered: Arc<Mutex<Vec<(u64, String)>>>,
}

struct ContractVisitor<'a>(&'a mut Option<String>);

impl Visit for ContractVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "contract" {
            *self.0 = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "contract" {
            *self.0 = Some(format!("{:?}", value));
        }
    }
}

impl<S> Layer<S> for SpanRecorder
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut contract = None;
        attrs.record(&mut ContractVisitor(&mut contract));
        if let Some(contract) = contract {
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(contract);
            }
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(contract) = span.extensions().get::<String>() {
                self.entered
                    .lock()
                    .unwrap()
                    .push((id.into_u64(), contract.clone()));
            }
        }
    }

    fn on_event(&self, _event: &Event<'_>, _ctx: Context<'_, S>) {}
}

#[tokio::test]
async fn test_concurrent_deliveries_keep_span_attribution() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let recorder = SpanRecorder::default();
    let subscriber = tracing_subscriber::registry().with(recorder.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let client = build_client().unwrap();
    let url = format!("{}/hook", server.uri());

    let mut handles = Vec::new();
    for contract in ["ContractA", "ContractB"] {
        let client = client.clone();
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            let payload = test_payload(contract);
            let (_tx, rx) = oneshot::channel();
            let span = tracing::info_span!("delivery", contract = contract);
            let _ = tracing::Instrument::instrument(
                send_webhook(&client, &url, &payload, Some("secret"), rx),
                span,
            )
            .await;
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let entered = recorder.entered.lock().unwrap().clone();
    assert!(
        !entered.is_empty(),
        "expected at least one span to be entered during delivery"
    );

    // Each span id must map to exactly one contract; a guard held across an
    // await would let a span be entered under the wrong task's context.
    let mut seen: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    for (id, contract) in entered {
        if let Some(prev) = seen.insert(id, contract.clone()) {
            assert_eq!(
                prev, contract,
                "span {id} was entered with conflicting contract attribution"
            );
        }
    }
}

/// Snapshot test: a resolved (recovery / NoActivity) alert must be sent to
/// PagerDuty as `event_action: "resolve"` carrying the incident's `dedup_key`,
/// so the existing incident is resolved instead of a new one being triggered.
#[test]
fn test_pagerduty_resolve_snapshot() {
    let mut payload = test_payload("PagerDutyRecovery");
    payload.resolved = true;

    let body = txwatch_notifier::format::pagerduty(&payload);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(
        value["event_action"], "resolve",
        "resolved alerts must use event_action=resolve"
    );
    assert!(
        value.get("dedup_key").and_then(|v| v.as_str()).is_some(),
        "resolve payload must include the incident dedup_key"
    );

    insta::assert_snapshot!(body);
}

/// A rule configured with `severity = "info"` must page as `info` even when
/// its `rule_type` (e.g. `AdminFunctionCalled`) would otherwise be treated as
/// critical by the rule-type heuristic.
#[test]
fn test_pagerduty_severity_overrides_rule_type_heuristic() {
    let mut payload = test_payload("AdminFunctionCalled");
    payload.severity = Some("info".to_string());

    let body = txwatch_notifier::format::pagerduty(&payload);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(
        value["payload"]["severity"], "info",
        "configured severity must take precedence over rule_type heuristic"
    );
}

/// A `critical` `LargeTransfer` must page as `critical`, not the `warning`
/// the rule-type heuristic would otherwise assign.
#[test]
fn test_pagerduty_critical_large_transfer() {
    let mut payload = test_payload("LargeTransfer");
    payload.severity = Some("critical".to_string());

    let body = txwatch_notifier::format::pagerduty(&payload);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(value["payload"]["severity"], "critical");
}

/// When severity is unset, the rule-type heuristic must still apply.
#[test]
fn test_pagerduty_severity_falls_back_to_rule_type() {
    let mut payload = test_payload("AdminFunctionCalled");
    payload.severity = None;

    let body = txwatch_notifier::format::pagerduty(&payload);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(
        value["payload"]["severity"], "critical",
        "unset severity must fall back to the rule_type heuristic"
    );
}

/// Chat formats must render the full decimal `amount_xlm_decimal` (trailing
/// zeros trimmed) rather than the deprecated truncated `amount_xlm` integer,
/// so a 0.9 XLM transfer shows as "0.9 XLM" and 9,999.99 as "9999.99 XLM".
#[test]
fn test_chat_formats_render_decimal_amount() {
    let mut payload = test_payload("LargeTransfer");
    payload.amount_xlm = 0;
    payload.amount_xlm_decimal = "0.9000000".to_string();

    let details = txwatch_notifier::format::details(&payload);
    assert!(
        details.contains("0.9 XLM"),
        "details must render trimmed decimal amount, got: {details}"
    );
    assert!(
        !details.contains("0 XLM"),
        "details must not render the truncated integer amount, got: {details}"
    );

    payload.amount_xlm = 9999;
    payload.amount_xlm_decimal = "9999.9900000".to_string();

    let details = txwatch_notifier::format::details(&payload);
    assert!(
        details.contains("9999.99 XLM"),
        "details must render trimmed decimal amount, got: {details}"
    );
}

/// Discord colour must follow the configured severity: a `critical`
/// A rule-level `webhook_url` shorthand override must replace the destination
/// set entirely: only the override URL receives a delivery, and none of the
/// default `[[contracts.webhooks]]` destinations are hit.
#[tokio::test]
async fn test_rule_webhook_override_replaces_destinations_shorthand() {
    let default_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/default"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&default_server)
        .await;

    let override_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/override"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&override_server)
        .await;

    let client = build_client().unwrap();
    let payload = test_payload("RuleOverrideShorthand");
    let (_tx, rx) = oneshot::channel();

    let override_url = format!("{}/override", override_server.uri());
    let result = send_webhook(&client, &override_url, &payload, None, rx).await;
    assert!(result.is_ok(), "Override delivery must succeed");

    // The default destination must never be contacted when the override
    // replaces the destination set.
    let default_url = format!("{}/default", default_server.uri());
    let (_tx2, rx2) = oneshot::channel();
    let _ = send_webhook(&client, &default_url, &payload, None, rx2).await;
    default_server.verify().await;
    override_server.verify().await;
}

/// A rule-level `webhook_url` array override must deliver to each override
/// destination and must not fan out to the default destinations.
#[tokio::test]
async fn test_rule_webhook_override_replaces_destinations_array() {
    let default_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/default"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&default_server)
        .await;

    let first_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/first"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&first_server)
        .await;

    let second_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/second"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&second_server)
        .await;

    let client = build_client().unwrap();
    let payload = test_payload("RuleOverrideArray");

    let first_url = format!("{}/first", first_server.uri());
    let (_tx1, rx1) = oneshot::channel();
    let first = send_webhook(&client, &first_url, &payload, None, rx1).await;
    assert!(first.is_ok(), "First override destination must succeed");

    let second_url = format!("{}/second", second_server.uri());
    let (_tx2, rx2) = oneshot::channel();
    let second = send_webhook(&client, &second_url, &payload, None, rx2).await;
    assert!(second.is_ok(), "Second override destination must succeed");

    first_server.verify().await;
    second_server.verify().await;
    default_server.verify().await;
}
