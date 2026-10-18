#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

//! txwatch-rules evaluates `AlertRule` conditions against enriched Stellar transactions
//! and constructs structured `AlertPayload` webhook bodies.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use txwatch_config::{AlertRule, FunctionMatchMode, RuleConfig, EVENT_TOPIC_WILDCARD};

// ── Constants ──────────────────────────────────────────────────────────────────

/// Maximum XLM supply in stroops: 50 billion XLM × 10^7 stroops/XLM
/// = 5 × 10^17 (500 quadrillion) stroops, well below u64::MAX (~1.8 × 10^19).
/// Parsed amounts and fees above this value cannot exist on the network, so
/// [`EnrichedTransaction::from_horizon`] discards them as malformed.
pub const MAX_XLM_SUPPLY_STROOPS: u64 = 500_000_000_000_000_000;

/// Returns `value` if it is a possible on-chain stroop amount, logging and
/// discarding anything above [`MAX_XLM_SUPPLY_STROOPS`].
fn sanitize_stroops(value: Option<u64>, field: &str, tx_hash: &str) -> Option<u64> {
    match value {
        Some(v) if v > MAX_XLM_SUPPLY_STROOPS => {
            tracing::warn!(
                tx = %tx_hash,
                field,
                value = v,
                max = MAX_XLM_SUPPLY_STROOPS,
                "parsed amount exceeds the total XLM supply — ignoring it"
            );
            None
        }
        other => other,
    }
}

// ── Horizon transaction shape ─────────────────────────────────────────────────

/// Raw Horizon transaction record as returned by the REST API.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HorizonTransaction {
    pub hash: String,
    pub created_at: String, // RFC 3339
    pub successful: bool,
    pub paging_token: String,
    /// Fee charged in stroops (Horizon returns this as a string).
    pub fee_charged: Option<String>,
    /// The Stellar account that submitted (signed) the transaction.
    /// For fee-bump transactions, this is the fee-bump source account.
    pub source_account: Option<String>,
    /// For fee-bump transactions, the inner source account.
    pub fee_account: Option<String>,
    /// Base64-encoded XDR transaction envelope.
    pub envelope_xdr: Option<String>,
    /// Base64-encoded XDR transaction result.
    pub result_xdr: Option<String>,
    // ── Issue #56: additional Horizon fields ──────────────────────────────────
    /// Ledger sequence number in which this transaction was included.
    pub ledger: Option<u32>,
    /// Source account address (G-address) of the transaction.
    /// Memo content as a string (may be absent for `MemoNone`).
    pub memo: Option<String>,
    /// Memo type: `"none"`, `"text"`, `"id"`, `"hash"`, `"return"`.
    pub memo_type: Option<String>,
    /// Total number of operations in the transaction.
    pub operation_count: Option<u32>,
}

// ── Contract events ───────────────────────────────────────────────────────────

/// A Soroban contract event emitted by a transaction, as returned by Soroban
/// RPC `getEvents` with `xdrFormat: "json"`. Topics and data are `ScVal`s in
/// their JSON form, e.g. `{"symbol": "transfer"}` or `{"address": "G…"}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContractEvent {
    /// Contract that emitted the event.
    pub contract_id: String,
    pub topics: Vec<Value>,
    pub data: Value,
}

impl ContractEvent {
    /// The first topic as a symbol, if it is one.
    fn first_symbol(&self) -> Option<&str> {
        self.topics.first()?.get("symbol")?.as_str()
    }

    /// Does this event match an `EventEmitted { topic, topics }` rule?
    /// `topic` must equal topic 0 as a symbol exactly; each entry of `topics`
    /// is compared positionally against topics 1.. (`"*"` matches anything).
    pub fn matches(&self, topic: &str, topics: &[String]) -> bool {
        if self.first_symbol() != Some(topic) {
            return false;
        }
        topics.iter().enumerate().all(|(i, pattern)| {
            pattern == EVENT_TOPIC_WILDCARD
                || self
                    .topics
                    .get(i + 1)
                    .is_some_and(|value| topic_value_matches(value, pattern))
        })
    }
}

/// Compare a topic's compact JSON rendering against `pattern`. A topic is
/// compared once per event, so the allocation is not worth avoiding.
#[allow(clippy::cmp_owned)]
fn cmp_rendered(value: &Value, pattern: &str) -> bool {
    value.to_string() == pattern
}

// Compare one `ScVal` topic against a config pattern. Single-key scalar
/// values (`{"symbol": "x"}`, `{"address": "G…"}`, `{"u32": 5}`, `{"i128": "-1"}`)
/// match the inner value's text; anything else matches its compact JSON.
fn topic_value_matches(value: &Value, pattern: &str) -> bool {
    let scalar = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    let inner = match value {
        Value::Object(map) if map.len() == 1 => map.values().next().and_then(scalar),
        other => scalar(other),
    };
    match inner {
        Some(text) => text == pattern,
        // A non-scalar topic matches its own compact JSON rendering.
        None => cmp_rendered(value, pattern),
    }
}

// ── ScVal decoding (issue #266) ───────────────────────────────────────────────

/// Decode a Soroban `ScVal` (in its JSON form) into a plain string for
/// argument matching. Handles the common scalar types: `address`, `symbol`,
/// `string`, `u32`, `i32`, `u64`, `i64`, `u128`, `i128`, `bool`. Anything else
/// falls back to its compact JSON rendering.
pub fn decode_scval(value: &Value) -> String {
    match value {
        Value::Object(map) if map.len() == 1 => {
            let (key, inner) = map.iter().next().expect("map has one entry");
            match (key.as_str(), inner) {
                ("address", Value::String(s))
                | ("symbol", Value::String(s))
                | ("string", Value::String(s)) => s.clone(),
                ("u32", _)
                | ("i32", _)
                | ("u64", _)
                | ("i64", _)
                | ("u128", _)
                | ("i128", _)
                | ("bool", _) => match inner {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    other => other.to_string(),
                },
                _ => value.to_string(),
            }
        }
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// A single decoded invocation argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodedArg {
    /// Zero-based position in the invocation's argument list.
    pub index: usize,
    /// Decoded textual value.
    pub value: String,
}

/// A Soroban contract function invocation with its decoded arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionInvocation {
    pub function_name: String,
    /// Decoded arguments in order.
    pub args: Vec<DecodedArg>,
}

impl FunctionInvocation {
    /// Build from a function name and the raw `parameters` array (each element
    /// is an `ScVal` in JSON form).
    pub fn from_parameters(function_name: String, parameters: &[Value]) -> Self {
        let args = parameters
            .iter()
            .enumerate()
            .map(|(index, value)| DecodedArg {
                index,
                value: decode_scval(value),
            })
            .collect();
        Self {
            function_name,
            args,
        }
    }

    /// The decoded value at `index`, if present.
    pub fn arg(&self, index: usize) -> Option<&str> {
        self.args
            .iter()
            .find(|a| a.index == index)
            .map(|a| a.value.as_str())
    }
}

// ── Enriched transaction ──────────────────────────────────────────────────────

/// A transaction enriched with Soroban-specific fields extracted from the
/// Horizon `operations` sub-resource JSON (returned inline via `join=operations`
/// or fetched separately). We keep this as a plain struct so rule evaluation
/// stays pure and testable without network calls.
#[derive(Debug, Clone)]
pub struct EnrichedTransaction {
    pub hash: String,
    pub timestamp: DateTime<Utc>,
    pub successful: bool,
    pub paging_token: String,
    /// All Soroban contract functions invoked in this transaction (may be multiple).
    pub function_names: Vec<String>,
    /// Decoded invocations (function name + arguments) for this transaction.
    /// Populated from the operation `parameters` array (issue #266).
    pub invocations: Vec<FunctionInvocation>,
    /// Transfer amount in stroops (1 XLM = 10_000_000 stroops), if detected.
    /// Uses u64 because the total XLM supply is ~50 billion XLM = 5 × 10^17
    /// (500 quadrillion) stroops ([`MAX_XLM_SUPPLY_STROOPS`]), well within
    /// u64::MAX (~1.8 × 10^19).
    pub amount_stroops: Option<u64>,
    /// Fee charged for this transaction in stroops.
    pub fee_charged_stroops: Option<u64>,
    /// The Stellar account that submitted the transaction (G-address).
    pub source_account: Option<String>,
    /// For fee-bump transactions, the inner source account.
    pub fee_account: Option<String>,
    // ── Issue #56: additional metadata fields ─────────────────────────────────
    /// Ledger sequence number in which this transaction was included.
    pub ledger: Option<u32>,
    /// Source account address (G-address) of the transaction.
    /// Memo content (absent for `MemoNone`).
    pub memo: Option<String>,
    /// Memo type: `"none"`, `"text"`, `"id"`, `"hash"`, `"return"`.
    pub memo_type: Option<String>,
    /// Total number of operations in the transaction.
    pub operation_count: Option<u32>,
    /// Contract events emitted by this transaction. Only populated when the
    /// contract has an `EventEmitted` rule (fetched from Soroban RPC).
    pub events: Vec<ContractEvent>,
}

impl EnrichedTransaction {
    /// Build from a raw Horizon record plus optional Soroban operation details.
    pub fn from_horizon(
        tx: HorizonTransaction,
        function_names: Vec<String>,
        amount_stroops: Option<u64>,
        fee_charged_stroops: Option<u64>,
    ) -> Result<Self> {
        let timestamp = tx.created_at.parse::<DateTime<Utc>>().with_context(|| {
            format!(
                "cannot parse timestamp '{}' for tx {}",
                tx.created_at, tx.hash
            )
        })?;

        let fee_charged_stroops = fee_charged_stroops.or_else(|| {
            tx.fee_charged
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok())
        });

        Ok(Self {
            amount_stroops: sanitize_stroops(amount_stroops, "amount_stroops", &tx.hash),
            fee_charged_stroops: sanitize_stroops(
                fee_charged_stroops,
                "fee_charged_stroops",
                &tx.hash,
            ),
            hash: tx.hash,
            timestamp,
            successful: tx.successful,
            paging_token: tx.paging_token,
            function_names,
            invocations: Vec::new(),
            // The sanitized values set above are authoritative; the raw ones
            // are deliberately not repeated here.
            source_account: tx.source_account,
            fee_account: tx.fee_account,
            ledger: tx.ledger,
            memo: tx.memo,
            memo_type: tx.memo_type,
            operation_count: tx.operation_count,
            events: Vec::new(),
        })
    }

    /// Attach the contract events emitted by this transaction.
    pub fn with_events(mut self, events: Vec<ContractEvent>) -> Self {
        self.events = events;
        self
    }

    /// Attach decoded function invocations (with arguments).
    pub fn with_invocations(mut self, invocations: Vec<FunctionInvocation>) -> Self {
        self.invocations = invocations;
        self
    }
}

// ── AlertPayload ──────────────────────────────────────────────────────────────

/// The JSON body POSTed to the webhook URL when a rule fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AlertPayload {
    /// Schema version for this payload shape (issue #58).
    /// Receivers should use this to detect breaking changes.
    /// Additive changes (new optional fields) keep the same version;
    /// breaking changes (field removals or renames) bump it.
    pub schema_version: u32,
    /// Stable, deterministic alert identifier derived from
    /// (network, contract_id, tx hash, rule_type, rule params) via SHA-256
    /// (issue #57). Receivers should deduplicate on this value.
    /// Stable identifier of this alert (see [`alert_id`]): the same contract,
    /// transaction and rule always produce the same ID, so receivers can
    /// de-duplicate retries. Used as the PagerDuty `dedup_key`.
    #[serde(default)]
    pub alert_id: String,
    pub label: String,
    pub contract_id: String,
    pub network: String,
    /// Stable machine-readable rule variant (e.g. `"LargeTransfer"`).
    pub rule_type: String,
    pub rule_triggered: String,
    /// Transaction hash, or `null` for synthetic alerts (e.g. `NoActivity`).
    pub transaction_hash: String,
    /// First invoked function name (backward-compat singular field).
    pub function_name: Option<String>,
    /// All invoked function names in this transaction.
    pub function_names: Vec<String>,
    /// Decoded function invocations (name + arguments) for this transaction.
    /// Empty when the operation parameters were not available (issue #266).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub function_args: Vec<FunctionInvocation>,
    /// Amount in whole XLM (truncated, stroops / 10_000_000), present for
    /// LargeTransfer. Kept for backward compatibility — use `amount_xlm_decimal`
    /// for precise accounting (e.g. 9,999.99 XLM appears here as `9999`).
    #[serde(rename = "amount_xlm")]
    pub amount_xlm: Option<u64>,
    /// Transfer amount in stroops (1 XLM = 10_000_000 stroops).
    pub amount_stroops: Option<u64>,
    /// Transfer amount as a decimal string with 7 fractional digits
    /// (e.g. `"9999.9900000"`), or `null` when no amount is present.
    pub amount_xlm_decimal: Option<String>,
    /// Fee charged in stroops.
    pub fee_charged_stroops: Option<u64>,
    /// The Stellar account that submitted the transaction (G-address).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_account: Option<String>,
    /// Optional severity level from the rule definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    /// Unix timestamp (seconds).
    pub timestamp: i64,
    /// ISO 8601 timestamp string.
    pub timestamp_iso: String,
    pub horizon_link: String,
    /// Stellar Expert explorer link for the transaction.
    pub explorer_link: String,
    /// Effective webhook URL for this alert (may be rule-level override).
    #[serde(skip)]
    pub effective_webhook_url: Option<String>,
    /// Effective webhook secret for this alert (may be rule-level override).
    #[serde(skip)]
    pub effective_webhook_secret: Option<String>,
    // ── Issue #56: additional metadata fields ─────────────────────────────────
    /// Ledger sequence number in which this transaction was included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger: Option<u32>,
    /// Memo content (absent for `MemoNone`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
    /// Memo type: `"none"`, `"text"`, `"id"`, `"hash"`, `"return"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memo_type: Option<String>,
    /// Total number of operations in the transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_count: Option<u32>,
    /// `true` for a recovery alert (e.g. activity resumed after `NoActivity`).
    /// `false` (the default) for an incident alert.
    #[serde(default)]
    pub resolved: bool,
    /// Contract events that matched an `EventEmitted` rule (topics and data);
    /// empty for every other rule type.
    #[serde(default)]
    pub matched_events: Vec<ContractEvent>,
    /// Number of matches of this rule that were suppressed by its
    /// `cooldown_seconds` since the previous alert was sent. 0 when the rule
    /// has no cooldown or nothing was suppressed.
    #[serde(default)]
    pub suppressed_count: u64,
    /// Observed match count for a `Rate` rule breach (issue #267). `None` for
    /// every other rule type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_count: Option<u64>,
    /// `true` only for synthetic payloads sent by `txwatch test-webhook`;
    /// omitted from the JSON otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test: bool,
}

/// Deterministic alert ID: the first 32 hex characters (128 bits) of
/// SHA-256 over `contract_id`, `transaction_hash` and `rule_triggered`.
pub fn alert_id(contract_id: &str, transaction_hash: &str, rule_triggered: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in [contract_id, transaction_hash, rule_triggered] {
        hasher.update(part.as_bytes());
        // Separator so ("ab", "c") and ("a", "bc") differ.
        hasher.update([0u8]);
    }
    let mut id = hex::encode(hasher.finalize());
    id.truncate(32);
    id
}

// ── Rule evaluation ───────────────────────────────────────────────────────────

/// Evaluate all rules for one contract against one transaction.
/// Context passed to [`evaluate`] to identify the contract being evaluated
/// and provide the link base URLs needed to build webhook payloads.
///
/// Using a struct instead of five positional `&str` parameters prevents
/// argument-order bugs (e.g. swapping `horizon_base` and `explorer_base`).
///
/// # Example
/// ```
/// use txwatch_rules::EvalContext;
/// use txwatch_config::{AlertRule, Network, RuleConfig, WatchedContract};
///
/// let contract = WatchedContract {
///     label: "My Oracle".into(),
///     contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4".into(),
///     network: Network::Testnet,
///     rules: vec![RuleConfig {
///         rule: AlertRule::AnyTransaction,
///         cooldown_seconds: None,
///     }],
///     webhook_url: Some("https://hooks.example.com/hook".into()),
///     webhook_secret: None,
///     webhook_format: Default::default(),
///     webhook_headers: Default::default(),
///     webhook_routing_key: None,
///     webhooks: Vec::new(),
///     poll_interval_seconds: None,
///     enabled: true,
///     soroban_rpc_url: None,
///     batch_alerts: false,
///     horizon_base_url_override: None,
/// };
/// let ctx = EvalContext::from_contract(&contract);
/// assert_eq!(ctx.network, "testnet");
/// ```
#[derive(Debug, Clone)]
pub struct EvalContext<'a> {
    pub label: &'a str,
    pub contract_id: &'a str,
    pub network: &'a str,
    pub horizon_base: &'a str,
    /// Explorer base URL for the network; `None` for custom networks that
    /// have no configured explorer (links will fall back to `horizon_link`).
    pub explorer_base: Option<&'a str>,
}

impl<'a> EvalContext<'a> {
    /// Convenience constructor: derive all fields from a [`WatchedContract`].
    /// Callers that need to override the Horizon base URL (e.g. tests using a
    /// mock server) should fill in the fields manually instead.
    pub fn from_contract(contract: &'a txwatch_config::WatchedContract) -> Self {
        let horizon_base = contract
            .horizon_base_url_override
            .as_deref()
            .unwrap_or_else(|| contract.network.horizon_base_url());
        Self {
            label: &contract.label,
            contract_id: &contract.contract_id,
            network: contract.network.as_str(),
            horizon_base,
            explorer_base: contract.network.explorer_base_url(),
        }
    }
}

/// Shared, thread-safe counter map used to rate-limit repeated evaluation
/// warnings. Keyed by `"<contract_id>:<rule_label>"`.
#[derive(Debug, Default, Clone)]
pub struct WarningSuppressor(Arc<Mutex<HashMap<String, u64>>>);

impl WarningSuppressor {
    /// Returns `true` if the warning for this key should be emitted (i.e. the
    /// first occurrence or every 100th recurrence).
    pub fn should_warn(&self, key: &str) -> bool {
        if let Ok(mut map) = self.0.lock() {
            let count = map.entry(key.to_owned()).or_insert(0);
            *count += 1;
            *count == 1 || *count % 100 == 0
        } else {
            true // lock poisoned — always warn rather than silently drop
        }
    }

    /// Returns how many times a warning has been suppressed for the given key.
    #[cfg(test)]
    pub fn suppressed_count(&self, key: &str) -> u64 {
        self.0
            .lock()
            .ok()
            .and_then(|m| m.get(key).copied())
            .unwrap_or(0)
    }
}

/// Evaluate all per-transaction rules for one contract against one transaction.
/// Returns one `AlertPayload` per matching rule.
/// Never panics — errors in individual rule evaluation are logged and skipped.
/// Repeated errors for the same rule are suppressed after the first occurrence
/// (see [`WarningSuppressor`]).
///
/// Pass `suppressor` as `None` to use a one-shot suppressor (appropriate for
/// replay and tests). The poller keeps a per-contract suppressor to
/// deduplicate repeated warnings across poll cycles.
pub fn evaluate<R: AsRef<AlertRule> + RuleOverrides>(
    ctx: &EvalContext<'_>,
    rules: &[R],
    tx: &EnrichedTransaction,
    suppressor: Option<&WarningSuppressor>,
) -> Vec<AlertPayload> {
    let label = ctx.label;
    let contract_id = ctx.contract_id;
    let network = ctx.network;
    // #45: Trim trailing slashes so callers that pass "https://host/" do not
    // produce double slashes in the generated links.
    let horizon_base = ctx.horizon_base.trim_end_matches('/');
    let horizon_link = format!("{}/transactions/{}", horizon_base, tx.hash);
    let explorer_link = match ctx.explorer_base {
        Some(base) => format!("{}/tx/{}", base.trim_end_matches('/'), tx.hash),
        None => horizon_link.clone(),
    };
    let timestamp = tx.timestamp.timestamp();
    let timestamp_iso = tx.timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let _local_suppressor;
    let suppressor = match suppressor {
        Some(s) => s,
        None => {
            _local_suppressor = WarningSuppressor::default();
            &_local_suppressor
        }
    };

    // #46: Compute both forms of the amount once per transaction.
    let amount_xlm = tx.amount_stroops.map(|s| s / 10_000_000);
    let amount_xlm_decimal = tx.amount_stroops.map(|s| {
        let whole = s / 10_000_000;
        let frac = s % 10_000_000;
        format!("{}.{:07}", whole, frac)
    });

    rules
        .iter()
        .filter(|entry| entry.enabled())
        .filter_map(|entry| {
            let rule: &AlertRule = entry.as_ref();
            match eval_rule(rule, tx) {
                Ok(true) => Some(AlertPayload {
                    schema_version: 1,
                    alert_id: alert_id(contract_id, &tx.hash, &rule.label()),
                    label: label.to_string(),
                    contract_id: contract_id.to_string(),
                    network: network.to_string(),
                    // #44: Use the single canonical implementations from txwatch-config.
                    rule_type: rule.rule_type().to_string(),
                    rule_triggered: rule.label(),
                    transaction_hash: tx.hash.clone(),
                    function_name: tx.function_names.first().cloned(),
                    function_names: tx.function_names.clone(),
                    function_args: tx.invocations.clone(),
                    amount_xlm,
                    amount_stroops: tx.amount_stroops,
                    amount_xlm_decimal: amount_xlm_decimal.clone(),
                    fee_charged_stroops: tx.fee_charged_stroops,
                    source_account: tx.source_account.clone(),
                    // Per-rule delivery overrides, absent unless the rule sets them.
                    severity: entry.severity().map(|s| s.to_string()),
                    effective_webhook_url: entry.webhook_url().map(str::to_owned),
                    effective_webhook_secret: entry.webhook_secret().map(str::to_owned),
                    timestamp,
                    timestamp_iso: timestamp_iso.clone(),
                    horizon_link: horizon_link.clone(),
                    explorer_link: explorer_link.clone(),
                    ledger: tx.ledger,
                    memo: tx.memo.clone(),
                    memo_type: tx.memo_type.clone(),
                    operation_count: tx.operation_count,
                    resolved: false,
                    matched_events: matched_events(rule, tx),
                    suppressed_count: 0,
                    observed_count: None,
                    test: false,
                }),
                Ok(false) => None,
                Err(e) => {
                    // A broken rule would otherwise warn on every poll cycle.
                    let key = format!("{}:{}", ctx.contract_id, rule.label());
                    if suppressor.should_warn(&key) {
                        tracing::warn!(
                            tx = %tx.hash,
                            rule = %rule.label(),
                            error = %e,
                            "rule evaluation error — skipping"
                        );
                    }
                    None
                }
            }
        })
        .collect()
}

/// Per-rule delivery settings that ride alongside the rule itself. A bare
/// [`AlertRule`] and a `RuleConfig` have none; a `RuleEntry` (used for the
/// nested rules of a composite) carries the enable flag, the webhook overrides
/// and the severity.
pub trait RuleOverrides {
    /// `false` skips the rule entirely.
    fn enabled(&self) -> bool {
        true
    }
    /// Webhook URL this rule overrides the contract's with.
    fn webhook_url(&self) -> Option<&str> {
        None
    }
    /// Webhook secret this rule overrides the contract's with.
    fn webhook_secret(&self) -> Option<&str> {
        None
    }
    /// Severity carried on the alert payload.
    fn severity(&self) -> Option<&txwatch_config::Severity> {
        None
    }
}

impl<T: RuleOverrides + ?Sized> RuleOverrides for &T {
    fn enabled(&self) -> bool {
        (**self).enabled()
    }
    fn webhook_url(&self) -> Option<&str> {
        (**self).webhook_url()
    }
    fn webhook_secret(&self) -> Option<&str> {
        (**self).webhook_secret()
    }
    fn severity(&self) -> Option<&txwatch_config::Severity> {
        (**self).severity()
    }
}

impl RuleOverrides for AlertRule {}
impl RuleOverrides for txwatch_config::RuleConfig {}

impl RuleOverrides for txwatch_config::RuleEntry {
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn webhook_url(&self) -> Option<&str> {
        self.webhook_url.as_deref()
    }
    fn webhook_secret(&self) -> Option<&str> {
        self.webhook_secret.as_deref()
    }
    fn severity(&self) -> Option<&txwatch_config::Severity> {
        self.severity.as_ref()
    }
}

// NOTE: When adding a new AlertRule variant, update both `eval_rule()` in this
// file and `AlertRule::label()` / `AlertRule::rule_type()` in txwatch-config together.
// Rust's exhaustive matching catches missing arms in eval_rule automatically.
fn eval_rule(rule: &AlertRule, tx: &EnrichedTransaction) -> Result<bool> {
    Ok(match rule {
        AlertRule::AnyTransaction => true,

        AlertRule::TransactionFailed => !tx.successful,

        // #43: threshold_stroops is pre-computed once during validation; no
        // per-evaluation multiplication or overflow path needed.
        AlertRule::LargeTransfer {
            threshold_stroops, ..
        } => tx
            .amount_stroops
            .map(|s| s >= *threshold_stroops)
            .unwrap_or(false),

        AlertRule::FunctionCalled {
            function_name,
            match_mode,
            args,
        } => {
            // #266: A function matches when its name matches the mode AND every
            // configured argument constraint holds. When `args` is empty this
            // reduces to the original name-only behaviour.
            let name_matches = |f: &str| match match_mode {
                FunctionMatchMode::Exact => f == function_name.as_str(),
                FunctionMatchMode::Prefix => f.starts_with(function_name.as_str()),
                FunctionMatchMode::Glob => glob_match(function_name, f),
            };
            if args.is_empty() {
                tx.function_names.iter().any(|f| name_matches(f))
            } else {
                // Argument constraints require decoded invocations.
                tx.invocations.iter().any(|inv| {
                    name_matches(&inv.function_name)
                        && args.iter().all(|constraint| {
                            inv.arg(constraint.index)
                                .map(|v| v == constraint.eq.as_str())
                                .unwrap_or(false)
                        })
                })
            }
        }

        AlertRule::AdminFunctionCalled { function_names } => tx.function_names.iter().any(|f| {
            let f_lower = f.to_lowercase();
            function_names.iter().any(|n| n.to_lowercase() == f_lower)
        }),

        AlertRule::HighFee {
            threshold_stroops, ..
        } => tx
            .fee_charged_stroops
            .map(|f| f >= *threshold_stroops)
            .unwrap_or(false),

        AlertRule::SourceAccount { allow, deny } => {
            match tx.source_account.as_deref() {
                None => false,
                Some(src) => {
                    let in_allow = allow.iter().any(|a| a == src);
                    let in_deny = deny.iter().any(|d| d == src);
                    if !allow.is_empty() {
                        // `allow` gates the rule; an address on the `deny`
                        // fire-list still fires, so it can carve out exceptions.
                        in_allow || in_deny
                    } else if !deny.is_empty() {
                        // With no gate, `deny` alone decides.
                        in_deny
                    } else {
                        true
                    }
                }
            }
        }

        // #268: Memo rule. `memo_type` (when set) must match the transaction's
        // memo type; `equals` / `prefix` constrain the memo text.
        AlertRule::Memo {
            memo_type,
            equals,
            prefix,
        } => {
            if let Some(required_type) = memo_type {
                if tx.memo_type.as_deref() != Some(required_type.as_str()) {
                    return Ok(false);
                }
            }
            let memo = match tx.memo.as_deref() {
                Some(m) => m,
                None => return Ok(false),
            };
            if let Some(eq) = equals {
                if memo != eq.as_str() {
                    return Ok(false);
                }
            }
            if let Some(p) = prefix {
                if !memo.starts_with(p.as_str()) {
                    return Ok(false);
                }
            }
            true
        }

        AlertRule::All { rules } => {
            for entry in rules {
                if !entry.enabled {
                    continue;
                }
                if !eval_rule(&entry.rule, tx)? {
                    return Ok(false);
                }
            }
            true
        }

        AlertRule::Any { rules } => {
            for entry in rules {
                if !entry.enabled {
                    continue;
                }
                if eval_rule(&entry.rule, tx)? {
                    return Ok(true);
                }
            }
            false
        }

        AlertRule::Not { rule: inner } => !eval_rule(&inner.rule, tx)?,
        // NoActivity is a poll-cycle-level rule evaluated by `check_no_activity`,
        // not a per-transaction rule.  It never fires here.
        AlertRule::NoActivity { .. } => false,
        AlertRule::EventEmitted { topic, topics } => {
            tx.events.iter().any(|e| e.matches(topic, topics))
        }
        // #267: Rate is a stateful, windowed rule evaluated by `RateTracker`,
        // not a per-transaction rule. It never fires here.
        AlertRule::Rate { .. } => false,
    })
}

/// The events an `EventEmitted` rule matched; empty for other rules.
fn matched_events(rule: &AlertRule, tx: &EnrichedTransaction) -> Vec<ContractEvent> {
    match rule {
        AlertRule::EventEmitted { topic, topics } => tx
            .events
            .iter()
            .filter(|e| e.matches(topic, topics))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

// NOTE: When adding a new AlertRule variant, update both `eval_rule()` and
// `rule_label()` together. Rust's exhaustive matching catches missing arms,
// but this convention should be preserved for new rule variants.
fn rule_label(rule: &AlertRule) -> String {
    match rule {
        AlertRule::AnyTransaction => "AnyTransaction".into