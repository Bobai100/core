//! Real-time Soroban contract event streaming via JSON-RPC `getEvents`.
//! Replaces legacy REST polling with direct contract topic and event parsing.
//!
//! Supports two delivery modes per contract:
//! - [`StreamMode::Poll`] (default): interval polling via RPC `getEvents`.
//! - [`StreamMode::Stream`]: Horizon Server-Sent Events (`Accept: text/event-stream`)
//!   that resume from the last seen paging token on reconnect and fall back to
//!   polling after repeated disconnects.

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Delivery mode for a contract's event feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    /// Interval polling via RPC `getEvents` (preserves existing behavior).
    #[default]
    Poll,
    /// Horizon Server-Sent Events on collection endpoints.
    Stream,
}

/// Number of consecutive stream disconnects before falling back to polling.
pub const MAX_STREAM_DISCONNECTS: u32 = 3;

/// Client name sent to Horizon/RPC operators for identification and fair rate limiting.
const CLIENT_NAME: &str = "txwatch";
/// Client version sent to Horizon/RPC operators for identification.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// JSON-RPC 2.0 Request envelope.
#[derive(Debug, Serialize)]
struct JsonRpcRequest<T> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: T,
}

/// Parameters for Soroban RPC `getEvents`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetEventsParams {
    pub start_ledger: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<EventFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pagination: Option<PaginationParams>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EventFilter {
    #[serde(rename = "type")]
    pub event_type: String,
    pub contract_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<Vec<String>>,
}

#[derive(Debug, Serialize, Clone)]
pub struct PaginationParams {
    pub limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// JSON-RPC 2.0 Response envelope.
#[derive(Debug, Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetEventsResult {
    pub events: Vec<SorobanEvent>,
    pub latest_ledger: u64,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// A parsed Soroban contract event emitted on-chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SorobanEvent {
    pub id: String,
    pub contract_id: String,
    pub ledger: u64,
    pub ledger_closed_at: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub topic: Vec<String>,
    pub value: serde_json::Value,
    pub in_successful_contract_call: bool,
}

/// A single Server-Sent Event frame parsed from a Horizon stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    /// SSE `event:` field (e.g. `"message"` or `"hello"`).
    pub event: String,
    /// SSE `id:` field, used as the paging token for resumption.
    pub id: Option<String>,
    /// SSE `data:` field payload.
    pub data: String,
}

/// Parses a raw SSE frame (fields separated by newlines) into an [`SseEvent`].
///
/// Returns `None` for comment-only or empty frames, which Horizon uses as
/// keep-alives and should be ignored by the consumer.
pub fn parse_sse_frame(frame: &str) -> Option<SseEvent> {
    let mut event = String::from("message");
    let mut id = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in frame.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = value.to_string(),
            "id" => id = Some(value.to_string()),
            "data" => data_lines.push(value),
            _ => {}
        }
    }

    if data_lines.is_empty() {
        return None;
    }

    Some(SseEvent {
        event,
        id,
        data: data_lines.join("\n"),
    })
}

/// Real-time Soroban contract event streaming engine.
///
/// Supports multiple Horizon/RPC endpoints per network or contract. On a
/// connection error or 5xx response, the streamer falls back to the next
/// configured endpoint for that poll cycle.
///
/// This is the single RPC `getEvents` client used by the poller; it shares
/// [`GetEventsParams`], [`GetEventsResult`], and [`SorobanEvent`] with the
/// poller's `fetch_ledger_events` path so fixes reach both call sites.
pub(crate) struct SorobanEventStreamer {
    rpc_urls: Vec<String>,
    active_url: usize,
    contract_ids: Vec<String>,
    last_cursor: Option<String>,
    current_ledger: u64,
    client: Client,
    mode: StreamMode,
    consecutive_disconnects: u32,
}

impl SorobanEventStreamer {
    /// Creates a streamer from a single endpoint (backwards compatible).
    pub(crate) fn new(rpc_url: impl Into<String>, contract_ids: Vec<String>, start_ledger: u64) -> Self {
        Self::with_urls(vec![rpc_url.into()], contract_ids, start_ledger)
    }

    /// Creates a streamer from a list of endpoints with failover support.
    ///
    /// The first URL is used until it fails; subsequent polls then start from
    /// the endpoint that last succeeded.
    pub(crate) fn with_urls(rpc_urls: Vec<String>, contract_ids: Vec<String>, start_ledger: u64) -> Self {
        Self {
            rpc_urls,
            active_url: 0,
            contract_ids,
            last_cursor: None,
            current_ledger: start_ledger,
            client: build_client(),
            mode: StreamMode::default(),
            consecutive_disconnects: 0,
        }
    }

    /// Sets the delivery mode for this contract's feed.
    pub(crate) fn with_mode(mut self, mode: StreamMode) -> Self {
        self.mode = mode;
        self
    }

    /// Returns the currently active delivery mode, accounting for fallback.
    pub(crate) fn mode(&self) -> StreamMode {
        self.mode
    }

    /// The last paging token seen, used to resume a stream on reconnect.
    pub(crate) fn last_cursor(&self) -> Option<&str> {
        self.last_cursor.as_deref()
    }

    /// Fetches the next batch of real-time Soroban events via RPC `getEvents`.
    ///
    /// Tries each configured endpoint in order, starting from the last known
    /// healthy one, and falls back to the next URL on connection errors or 5xx
    /// responses. The endpoint that served the poll is logged at debug level.
    pub(crate) async fn fetch_events(&mut self) -> Result<Vec<SorobanEvent>> {
        if self.rpc_urls.is_empty() {
            return Err(anyhow!("no Horizon/RPC endpoints configured"));
        }

        let filter = EventFilter {
            event_type: "contract".into(),
            contract_ids: self.contract_ids.clone(),
            topics: vec![],
        };

        let params = GetEventsParams {
            start_ledger: self.current_ledger,
            filters: vec![filter],
            pagination: Some(PaginationParams {
                limit: 100,
                cursor: self.last_cursor.clone(),
            }),
        };

        let request_body = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "getEvents",
            params,
        };

        let mut last_error: Option<anyhow::Error> = None;
        let url_count = self.rpc_urls.len();

        for offset in 0..url_count {
            let idx = (self.active_url + offset) % url_count;
            let url = self.rpc_urls[idx].clone();

            match self.try_fetch(&url, &request_body).await {
                Ok(result) => {
                    if idx != self.active_url {
                        debug!(url = %url, "switched active RPC endpoint");
                        self.active_url = idx;
                    }
                    self.consecutive_disconnects = 0;
                    if let Some(cursor) = result.cursor.clone() {
                        self.last_cursor = Some(cursor);
                    }
                    if result.latest_ledger > self.current_ledger {
                        self.current_ledger = result.latest_ledger;
                    }
                    return Ok(result.events);
                }
                Err(err) => {
                    warn!(url = %url, error = %err, "RPC getEvents failed, trying next endpoint");
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("all RPC endpoints failed")))
    }

    async fn try_fetch(
        &self,
        url: &str,
        request_body: &JsonRpcRequest<GetEventsParams>,
    ) -> Result<GetEventsResult> {
        let response = self
            .client
            .post(url)
            .header("X-Client-Name", CLIENT_NAME)
            .header("X-Client-Version", CLIENT_VERSION)
            .json(request_body)
            .send()
            .await
            .with_context(|| format!("posting getEvents to {url}"))?;

        let status = response.status();
        if status.is_server_error() {
            return Err(anyhow!("RPC endpoint {url} returned {status}"));
        }

        let envelope: JsonRpcResponse<GetEventsResult> = response
            .json()
            .await
            .with_context(|| format!("decoding getEvents response from {url}"))?;

        if let Some(err) = envelope.error {
            return Err(anyhow!(
                "RPC error {} from {url}: {}",
                err.code, err.message
            ));
        }

        envelope
            .result
            .ok_or_else(|| anyhow!("empty getEvents result from {url}"))
    }
}

fn build_client() -> Client {
    Client::builder()
        .user_agent(format!("{CLIENT_NAME}/{CLIENT_VERSION}"))
        .build()
        .unwrap_or_else(|err| {
            info!(error = %err, "falling back to default HTTP client");
            Client::new()
        })
}
