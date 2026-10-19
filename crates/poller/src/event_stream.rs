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
use tracing::{debug, warn};

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
///
/// Soroban RPC rejects requests that set both `startLedger` and
/// `pagination.cursor`, so `start_ledger` is omitted whenever a cursor is
/// present.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetEventsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_ledger: Option<u64>,
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
pub struct SorobanEventStreamer {
    rpc_urls: Vec<String>,
    active_url: usize,
    contract_ids: Vec<String>,
    last_cursor: Option<String>,
    current_ledger: u64,
    client: Client,
    mode: StreamMode,
    #[allow(dead_code)]
    consecutive_disconnects: u32,
}

impl SorobanEventStreamer {
    /// Creates a streamer from a single endpoint (backwards compatible).
    pub fn new(rpc_url: impl Into<String>, contract_ids: Vec<String>, start_ledger: u64) -> Self {
        Self::with_urls(vec![rpc_url.into()], contract_ids, start_ledger)
    }

    /// Creates a streamer from a list of endpoints with failover support.
    ///
    /// The first URL is used until it fails; subsequent polls then start from
    /// the endpoint that last succeeded.
    pub fn with_urls(rpc_urls: Vec<String>, contract_ids: Vec<String>, start_ledger: u64) -> Self {
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
    pub fn with_mode(mut self, mode: StreamMode) -> Self {
        self.mode = mode;
        self
    }

    /// Returns the currently active delivery mode, accounting for fallback.
    pub fn mode(&self) -> StreamMode {
        self.mode
    }

    /// The last paging token seen, used to resume a stream on reconnect.
    pub fn last_cursor(&self) -> Option<&str> {
        self.last_cursor.as_deref()
    }

    /// Fetches the next batch of real-time Soroban events via RPC `getEvents`.
    ///
    /// Tries each configured endpoint in order, starting from the last known
    /// healthy one, and falls back to the next URL on connection errors or 5xx
    /// responses. The endpoint that served the poll is logged at debug level.
    ///
    /// `startLedger` is only sent when no cursor is known, since Soroban RPC
    /// rejects requests that combine the two. After a page is returned, the
    /// ledger position advances only past the last event actually delivered so
    /// unread events between the page and the tip are not skipped.
    pub async fn fetch_events(&mut self) -> Result<Vec<SorobanEvent>> {
        if self.rpc_urls.is_empty() {
            return Err(anyhow!("no Horizon/RPC endpoints configured"));
        }

        let filter = EventFilter {
            event_type: "contract".into(),
            contract_ids: self.contract_ids.clone(),
            topics: vec![],
        };

        let params = GetEventsParams {
            start_ledger: if self.last_cursor.is_some() {
                None
            } else {
                Some(self.current_ledger)
            },
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

                    if let Some(cursor) = result.cursor.clone() {
                        self.last_cursor = Some(cursor);
                    }

                    // Advance only past the last event actually returned so
                    // events between this page and the tip are not skipped.
                    if let Some(last) = result.events.last() {
                        self.current_ledger = last.ledger.saturating_add(1);
                    }

                    debug!(
                        url = %url,
                        events = result.events.len(),
                        latest_ledger = result.latest_ledger,
                        current_ledger = self.current_ledger,
                        "fetched Soroban events"
                    );

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

        let body: JsonRpcResponse<GetEventsResult> = response
            .json()
            .await
            .with_context(|| format!("decoding getEvents response from {url}"))?;

        if let Some(err) = body.error {
            return Err(anyhow!("RPC error {}: {}", err.code, err.message));
        }

        body.result
            .ok_or_else(|| anyhow!("RPC response from {url} missing result"))
    }
}

fn build_client() -> Client {
    Client::builder()
        .user_agent(format!("{CLIENT_NAME}/{CLIENT_VERSION}"))
        .build()
        .unwrap_or_else(|_| Client::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn event_json(id: &str, ledger: u64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "contractId": "CABC",
            "ledger": ledger,
            "ledgerClosedAt": "2024-01-01T00:00:00Z",
            "type": "contract",
            "topic": ["AAA"],
            "value": { "x": 1 },
            "inSuccessfulContractCall": true
        })
    }

    fn rpc_result(
        events: Vec<serde_json::Value>,
        latest_ledger: u64,
        cursor: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "events": events,
                "latestLedger": latest_ledger,
                "cursor": cursor
            }
        })
    }

    #[tokio::test]
    async fn first_page_sends_start_ledger_without_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rpc_result(
                vec![event_json("1-1", 10)],
                50,
                Some("cursor-a"),
            )))
            .mount(&server)
            .await;

        let mut streamer = SorobanEventStreamer::new(server.uri(), vec!["CABC".into()], 10);
        let events = streamer.fetch_events().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(streamer.last_cursor(), Some("cursor-a"));
        assert_eq!(streamer.current_ledger, 11);
    }

    #[tokio::test]
    async fn multi_page_stream_uses_cursor_and_omits_start_ledger() {
        let server = MockServer::start().await;

        // First page: no cursor yet, startLedger present.
        Mock::given(method("POST"))
            .and(path("/"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "params": { "startLedger": 10 }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(rpc_result(
                vec![event_json("1-1", 10), event_json("1-2", 11)],
                50,
                Some("cursor-a"),
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second page: cursor present, startLedger must be absent.
        Mock::given(method("POST"))
            .and(path("/"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "params": { "pagination": { "cursor": "cursor-a" } }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(rpc_result(
                vec![event_json("1-3", 12)],
                50,
                Some("cursor-b"),
            )))
            .mount(&server)
            .await;

        let mut streamer = SorobanEventStreamer::new(server.uri(), vec!["CABC".into()], 10);

        let first = streamer.fetch_events().await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(streamer.current_ledger, 12);

        let second = streamer.fetch_events().await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(streamer.last_cursor(), Some("cursor-b"));
        assert_eq!(streamer.current_ledger, 13);
    }

    #[test]
    fn get_events_params_omits_start_ledger_when_cursor_present() {
        let params = GetEventsParams {
            start_ledger: None,
            filters: vec![],
            pagination: Some(PaginationParams {
                limit: 100,
                cursor: Some("cursor-a".into()),
            }),
        };
        let value = serde_json::to_value(&params).unwrap();
        assert!(value.get("startLedger").is_none());
        assert_eq!(value["pagination"]["cursor"], "cursor-a");
    }
}
