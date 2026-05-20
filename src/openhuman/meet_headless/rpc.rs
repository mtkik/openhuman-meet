//! JSON-RPC handlers for the `meet_headless` domain.
//!
//! Two endpoints, mirroring the start/stop shape of `meet_agent`:
//!
//! - `start` — launch chromium, run the join flow, start caption watch.
//! - `stop`  — abort the watcher, kill the browser, return a summary.
//!
//! Validation lives here; the heavy lifting is in [`super::runner`].

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::openhuman::meet::ops::{validate_display_name, validate_meet_url};
use crate::rpc::RpcOutcome;

use super::runner::HeadlessSession;

const LOG_PREFIX: &str = "[meet-headless-rpc]";

#[derive(Debug, Clone, Deserialize)]
struct StartRequest {
    /// `request_id` minted by the caller. Same key the matching
    /// `meet_agent` session is opened under, so the rest of the
    /// pipeline (`push_caption`, future `push_listen_pcm`) can find it.
    request_id: String,
    /// The Meet URL to join. Validated via [`validate_meet_url`] so the
    /// headless runner can't be used as a generic chromium driver.
    meet_url: String,
    /// Guest display name typed into the "Your name" input. Trimmed
    /// and length-capped via [`validate_display_name`].
    display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct StopRequest {
    request_id: String,
}

pub async fn handle_start(params: Map<String, Value>) -> Result<Value, String> {
    let req: StartRequest = serde_json::from_value(Value::Object(params))
        .map_err(|e| format!("{LOG_PREFIX} invalid start params: {e}"))?;

    let url = validate_meet_url(&req.meet_url)
        .map_err(|e| format!("{LOG_PREFIX} {e}"))?;
    let display_name = validate_display_name(&req.display_name)
        .map_err(|e| format!("{LOG_PREFIX} {e}"))?;

    log::info!(
        "{LOG_PREFIX} start request_id={} meet_url={} display_name={}",
        req.request_id,
        url.as_str(),
        display_name
    );

    HeadlessSession::start(url.as_str(), &display_name, &req.request_id)
        .await
        .map_err(|e| format!("{LOG_PREFIX} start: {e}"))?;

    RpcOutcome::new(
        json!({
            "ok": true,
            "request_id": req.request_id,
            "meet_url": url.as_str(),
        }),
        vec![],
    )
    .into_cli_compatible_json()
}

pub async fn handle_stop(params: Map<String, Value>) -> Result<Value, String> {
    let req: StopRequest = serde_json::from_value(Value::Object(params))
        .map_err(|e| format!("{LOG_PREFIX} invalid stop params: {e}"))?;

    let summary = HeadlessSession::stop(&req.request_id)
        .await
        .map_err(|e| format!("{LOG_PREFIX} stop: {e}"))?;

    log::info!(
        "{LOG_PREFIX} stop request_id={} captions_seen={}",
        summary.request_id,
        summary.captions_seen
    );

    RpcOutcome::new(
        json!({
            "ok": true,
            "request_id": summary.request_id,
            "captions_seen": summary.captions_seen,
        }),
        vec![],
    )
    .into_cli_compatible_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_rejects_non_meet_url() {
        let mut params = Map::new();
        params.insert("request_id".into(), json!("rid"));
        params.insert("meet_url".into(), json!("https://evil.example.com/abc-defg-hij"));
        params.insert("display_name".into(), json!("Bot"));
        let err = handle_start(params).await.unwrap_err();
        assert!(
            err.contains("host"),
            "expected URL validation to reject non-meet host, got: {err}"
        );
    }

    #[tokio::test]
    async fn start_rejects_empty_display_name() {
        let mut params = Map::new();
        params.insert("request_id".into(), json!("rid"));
        params.insert(
            "meet_url".into(),
            json!("https://meet.google.com/abc-defg-hij"),
        );
        params.insert("display_name".into(), json!("   "));
        let err = handle_start(params).await.unwrap_err();
        assert!(
            err.contains("display_name"),
            "expected display_name validation, got: {err}"
        );
    }

    #[tokio::test]
    async fn stop_returns_not_found_for_unknown_session() {
        let mut params = Map::new();
        params.insert("request_id".into(), json!("does-not-exist"));
        let err = handle_stop(params).await.unwrap_err();
        assert!(err.contains("not found"), "expected not-found, got: {err}");
    }
}
