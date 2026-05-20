//! Headless caption monitor — periodically polls Meet's captions DOM
//! via `Runtime.evaluate` and feeds each new line into the in-process
//! `meet_agent` session.
//!
//! Ported from `app/src-tauri/recipes/google-meet/recipe.js` (the
//! page-side caption scraper that the Shell-side `caption_listener`
//! drains). The same `[jsname="tgaKEf"]` primary selector and `img[alt]`
//! / `[data-self-name]` speaker heuristics are kept — they've been
//! stable through several Meet redesigns.
//!
//! ## In-process delivery
//!
//! Unlike the Shell-side listener (which posts captions over loopback
//! JSON-RPC), we live inside Core. Skipping the RPC round-trip:
//! - removes a serialisation hop for hot-path string data,
//! - means the `meet_agent` session must already be open before the
//!   watcher starts — the runner ensures this.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::time::interval;

use super::cdp::CdpConn;

const LOG_PREFIX: &str = "[meet-headless-caption]";

/// Polling cadence — matches the Shell-side listener so latency and
/// CDP load are comparable between desktop and headless runners.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Bail out after this many consecutive `Runtime.evaluate` failures.
/// Usually means the page navigated away (call ended) or the renderer
/// crashed — same shape as the Shell-side cap.
const MAX_CONSECUTIVE_ERRORS: u32 = 30;

/// Per-session counter of captions we've forwarded. Lets
/// `meet_headless_stop` report a useful summary without dragging the
/// meet_agent registry into the response.
static COUNTERS: Lazy<Mutex<HashMap<String, u64>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// JS injected via `Runtime.evaluate` each tick. Returns the array of
/// caption rows currently rendered, applying the same icon/snackbar
/// filters as `recipe.js` so toolbar tooltips don't get logged as
/// transcript lines.
const DRAIN_SCRIPT: &str = r#"
(() => {
  function looksLikeIconLigature(text) {
    if (!text) return true;
    const t = text.trim();
    if (!t) return true;
    if (/^[a-z0-9_]+$/.test(t) && t.length < 40) return true;
    return false;
  }
  function looksLikeCaptionLine(text) {
    if (!text) return false;
    const t = text.trim();
    if (t.length < 3) return false;
    if (looksLikeIconLigature(t)) return false;
    if (t.length < 20 && !/\s/.test(t)) return false;
    if (/^([a-z]+)([A-Z][a-z]*)$/.test(t)) {
      const m = /^([a-z]+)([A-Z][a-z]*)$/.exec(t);
      if (m && m[2].toLowerCase() === m[1]) return false;
    }
    if (/\b[a-z]+_[a-z]+\b/.test(t)) return false;
    if (/Your meeting is safe|Your meeting's ready|Copy link|Meeting details|Add people|Add others|Jump to bottom|Jump to most recent/i.test(t)) return false;
    if (/([a-z]{3,})\1/i.test(t)) return false;
    return true;
  }
  function rowSpeaker(row) {
    try {
      const img = row.querySelector('img[alt]');
      if (img) {
        const alt = (img.getAttribute('alt') || '').trim();
        if (alt && alt.length > 1 && !looksLikeIconLigature(alt) && !/^avatar$/i.test(alt)) {
          return alt;
        }
      }
      const self = row.querySelector('[data-self-name]');
      if (self) {
        const name = (self.getAttribute('data-self-name') || '').trim();
        if (name) return name;
      }
      const spans = row.querySelectorAll('span');
      for (let i = 0; i < spans.length; i++) {
        const t = (spans[i].textContent || '').replace(/\s+/g, ' ').trim();
        if (!t) continue;
        if (looksLikeIconLigature(t)) continue;
        if (t.length > 40) continue;
        return t;
      }
    } catch (_) {}
    return '';
  }
  function rowText(row) {
    try {
      const full = (row.textContent || '').replace(/\s+/g, ' ').trim();
      if (!full) return '';
      const spans = row.querySelectorAll('span');
      let prefix = '';
      for (let i = 0; i < spans.length; i++) {
        const t = (spans[i].textContent || '').replace(/\s+/g, ' ').trim();
        if (t) { prefix = t; break; }
      }
      let stripped = full;
      if (prefix && full.toLowerCase().startsWith(prefix.toLowerCase())) {
        stripped = full.slice(prefix.length).trim();
      }
      stripped = stripped.replace(/\s*arrow_downward\s*Jump to bottom\s*$/i, '').trim();
      return stripped;
    } catch (_) { return ''; }
  }
  let region = null;
  try { region = document.querySelector('[jsname="tgaKEf"]'); } catch (_) {}
  if (!region) {
    try {
      const labelled = document.querySelectorAll('[role="region"][aria-label],[aria-label]');
      for (let i = 0; i < labelled.length; i++) {
        const lbl = (labelled[i].getAttribute('aria-label') || '').trim();
        if (/^(captions|sous-titres|untertitel|leyendas|字幕)$/i.test(lbl)) {
          region = labelled[i];
          break;
        }
      }
    } catch (_) {}
  }
  if (!region) return [];
  const out = [];
  try {
    const children = region.children;
    for (let i = 0; i < children.length; i++) {
      const row = children[i];
      const speaker = rowSpeaker(row);
      const text = rowText(row);
      if (!text) continue;
      if (!looksLikeCaptionLine(text)) continue;
      if (!speaker && text.length < 12) continue;
      out.push({ speaker: speaker, text: text });
    }
  } catch (_) {}
  return out;
})()
"#;

/// Spawn the polling loop. Returns a shutdown sender — drop or send
/// `()` on it to stop the watcher.
pub fn spawn_watcher(
    request_id: String,
    cdp: CdpConn,
    session_id: String,
) -> oneshot::Sender<()> {
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    // Initialise the counter so `take_seen_count` reports zero rather
    // than "session not found" for sessions that never saw a caption.
    COUNTERS
        .lock()
        .expect("caption counters poisoned")
        .insert(request_id.clone(), 0);

    let request_id_for_task = request_id.clone();
    tokio::spawn(async move {
        let mut tick = interval(POLL_INTERVAL);
        // Burn the first tick so the page has time to render the captions
        // region before our first drain (same lead-time as the Shell-side
        // listener uses for `__openhumanDrainCaptions`).
        tick.tick().await;
        let mut cdp = cdp;
        // Compare against the previous snapshot so we only forward a
        // caption line once even though the DOM keeps re-rendering it.
        let mut last_key = String::new();
        let mut errors: u32 = 0;
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    log::info!(
                        "{LOG_PREFIX} watcher shutdown request_id={request_id_for_task}"
                    );
                    break;
                }
                _ = tick.tick() => {
                    match drain_once(&mut cdp, &session_id).await {
                        Ok(rows) => {
                            errors = 0;
                            forward_new_rows(&request_id_for_task, &rows, &mut last_key);
                        }
                        Err(err) => {
                            errors += 1;
                            log::debug!(
                                "{LOG_PREFIX} drain err request_id={request_id_for_task} consec={errors} err={err}"
                            );
                            if errors >= MAX_CONSECUTIVE_ERRORS {
                                log::warn!(
                                    "{LOG_PREFIX} giving up after {errors} consecutive errors request_id={request_id_for_task}"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        }
    });

    shutdown_tx
}

/// Read the captions-seen counter for a session and remove it. Called
/// by the runner during stop.
pub fn take_seen_count(request_id: &str) -> u64 {
    COUNTERS
        .lock()
        .expect("caption counters poisoned")
        .remove(request_id)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptionRow {
    pub speaker: String,
    pub text: String,
}

async fn drain_once(cdp: &mut CdpConn, session_id: &str) -> Result<Vec<CaptionRow>, String> {
    let res = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": DRAIN_SCRIPT,
                "returnByValue": true,
                "awaitPromise": false,
            }),
            Some(session_id),
        )
        .await?;
    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(parse_caption_rows(&value))
}

/// Parse the array of `{speaker, text}` rows returned by `DRAIN_SCRIPT`.
/// Split out so tests can exercise it without a live chromium.
pub(crate) fn parse_caption_rows(value: &Value) -> Vec<CaptionRow> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|row| {
            let speaker = row
                .get("speaker")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let text = row.get("text").and_then(|v| v.as_str())?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            Some(CaptionRow { speaker, text })
        })
        .collect()
}

/// Forward any rows we haven't seen this tick to the in-process
/// meet_agent session. The "what's new" check is a single
/// serialise-and-compare on the JSON-key of the full snapshot, mirroring
/// `recipe.js`'s `lastCaptionsKey` logic — Meet re-renders the rolling
/// transcript every word, so byte-equality is the cheapest dedup.
fn forward_new_rows(request_id: &str, rows: &[CaptionRow], last_key: &mut String) {
    let key = match serde_json::to_string(
        &rows
            .iter()
            .map(|r| (r.speaker.as_str(), r.text.as_str()))
            .collect::<Vec<_>>(),
    ) {
        Ok(k) => k,
        Err(_) => return,
    };
    if key == *last_key {
        return;
    }
    *last_key = key;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let registry = crate::openhuman::meet_agent::session::registry();
    let mut forwarded: u64 = 0;
    for row in rows {
        let outcome =
            registry.with_session(request_id, |s| s.note_caption(&row.speaker, &row.text, now_ms));
        match outcome {
            Ok(_) => forwarded += 1,
            Err(err) => {
                // Session was closed underneath us (stop race) — log
                // and stop trying to push more rows this tick.
                log::debug!(
                    "{LOG_PREFIX} note_caption failed request_id={request_id} err={err}"
                );
                break;
            }
        }
    }

    if forwarded > 0 {
        if let Ok(mut counters) = COUNTERS.lock() {
            if let Some(entry) = counters.get_mut(request_id) {
                *entry += forwarded;
            }
        }
        log::debug!(
            "{LOG_PREFIX} forwarded {forwarded} caption rows request_id={request_id}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_speaker_and_text_rows() {
        let v = json!([
            {"speaker": "Alice", "text": "Hello there"},
            {"speaker": "Bob",   "text": "Hi Alice"}
        ]);
        let rows = parse_caption_rows(&v);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].speaker, "Alice");
        assert_eq!(rows[0].text, "Hello there");
        assert_eq!(rows[1].text, "Hi Alice");
    }

    #[test]
    fn skips_rows_with_empty_text() {
        let v = json!([
            {"speaker": "Alice", "text": "   "},
            {"speaker": "Bob",   "text": "real caption"},
        ]);
        let rows = parse_caption_rows(&v);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].speaker, "Bob");
    }

    #[test]
    fn missing_speaker_defaults_to_empty() {
        let v = json!([{"text": "no speaker here"}]);
        let rows = parse_caption_rows(&v);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].speaker, "");
    }

    #[test]
    fn non_array_value_yields_empty() {
        assert!(parse_caption_rows(&Value::Null).is_empty());
        assert!(parse_caption_rows(&json!({})).is_empty());
        assert!(parse_caption_rows(&json!("oops")).is_empty());
    }

    #[test]
    fn drops_rows_with_no_text_field() {
        let v = json!([{"speaker": "x"}, {"text": "ok now"}]);
        let rows = parse_caption_rows(&v);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "ok now");
    }
}
