//! Minimal Chrome DevTools Protocol (CDP) WebSocket client for the
//! headless Meet runner.
//!
//! Mirrors the shape of `app/src-tauri/src/cdp/conn.rs` (Shell side) but
//! lives in Core: the Shell crate cannot be linked here, so we keep a
//! self-contained client that depends only on `tokio-tungstenite` (already
//! in the workspace deps).
//!
//! Scope is intentionally narrow — request/response calls only. The
//! headless runner does not (yet) need an event pump; caption monitoring
//! is poll-based via `Runtime.evaluate`. If a later phase wants pushed
//! `Page.frameNavigated` / `Runtime.consoleAPICalled` events we'll grow
//! a `pump_events` method here, mirroring the Shell-side impl.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Round-trip timeout for a single CDP call during the setup phase.
/// Matches the Shell-side default (35 s) so cold-attach behavior is
/// consistent between desktop and headless runners.
const CALL_TIMEOUT: Duration = Duration::from_secs(35);

/// Single-tab CDP session — holds the page-level WebSocket plus the
/// attached `sessionId` so callers don't have to thread it through
/// every helper.
pub struct CdpConn {
    sink: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    stream: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    next_id: i64,
}

impl CdpConn {
    /// Open a WebSocket to a CDP endpoint. `ws_url` is either:
    /// - the browser-level URL printed by chromium on stderr
    ///   (`ws://127.0.0.1:PORT/devtools/browser/UUID`), or
    /// - a per-target URL returned by `/json` (`/devtools/page/UUID`).
    pub async fn open(ws_url: &str) -> Result<Self, String> {
        let (ws, _resp) = connect_async(ws_url)
            .await
            .map_err(|e| format!("ws connect {ws_url}: {e}"))?;
        let (sink, stream) = ws.split();
        Ok(Self {
            sink,
            stream,
            next_id: 1,
        })
    }

    /// Send a JSON-RPC call and wait for the matching response. Drops
    /// unrelated events and stale responses on the floor — safe only
    /// before an event pump takes over the read side, which is the only
    /// mode we use today.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut req = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session_id {
            req["sessionId"] = json!(s);
        }
        let body = serde_json::to_string(&req).map_err(|e| format!("encode: {e}"))?;
        self.sink
            .send(Message::Text(body))
            .await
            .map_err(|e| format!("ws send: {e}"))?;

        loop {
            let msg = tokio::time::timeout(CALL_TIMEOUT, self.stream.next())
                .await
                .map_err(|_| format!("ws read timeout (method={method})"))?
                .ok_or_else(|| format!("ws closed (method={method})"))?
                .map_err(|e| format!("ws recv: {e}"))?;
            let text = match msg {
                Message::Text(t) => t,
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                    continue
                }
                Message::Close(_) => return Err("ws closed".into()),
            };
            let v: Value = serde_json::from_str(&text).map_err(|e| format!("decode: {e}"))?;
            if v.get("id").and_then(|x| x.as_i64()) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                return Err(format!("cdp error (method={method}): {err}"));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

/// Convenience: pull the string field at `result.<path>` for callers
/// that don't want to thread `serde_json::Value` plumbing themselves.
pub fn result_str(value: &Value, path: &[&str]) -> Option<String> {
    let mut cur = value;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_str().map(|s| s.to_string())
}
