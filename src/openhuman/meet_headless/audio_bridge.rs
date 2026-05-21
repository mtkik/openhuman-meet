//! Audio bridge for the headless Meet runner — Phase 6.3.
//!
//! Two pipelines:
//! 1. **Capture (Meet → Core)**: Capture page audio via injected JS, poll
//!    from Rust, decode Base64→PCM16LE, push to `meet_agent` session.
//! 2. **Playback (Core → Meet)**: Poll `meet_agent` session for outbound
//!    PCM, encode to Base64, inject into page via `Runtime.evaluate`.
//!
//! We live inside Core, so we bypass RPC and call the session registry
//! directly via `crate::openhuman::meet_agent::session::registry()`.

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::json;
use tokio::sync::oneshot;
use tokio::time::{interval, MissedTickBehavior};

use super::cdp::CdpConn;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const LOG_PREFIX: &str = "[meet-headless-audio]";

/// Polling cadence — 100 ms matches the Shell-side speak pump interval.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bail out after this many consecutive CDP / decode failures.
const MAX_CONSECUTIVE_ERRORS: u32 = 30;

/// Expected samples per 100 ms tick at 16 kHz mono.
const SAMPLES_PER_TICK: usize = 1600;

// ---------------------------------------------------------------------------
// JS injection scripts
// ---------------------------------------------------------------------------

/// Injects `window.__openhuman_capture` with `start()` and `drain()`.
///
/// `start()` creates an AudioContext at 16 kHz and captures **page audio**
/// (other participants' voices) by monitoring all `<audio>` and `<video>`
/// elements on the page via `MediaElementAudioSourceNode`, falling back to
/// a silent stream when no media elements are present yet. Each element's
/// source is connected through a `ScriptProcessorNode` that accumulates
/// Int16 samples into a buffer.
///
/// `drain()` returns the accumulated samples as a Base64 string and
/// clears the buffer. Returns `""` when nothing has accumulated.
pub(crate) const AUDIO_CAPTURE_SETUP_JS: &str = r#"
(() => {
  if (window.__openhuman_capture) return;

  let ctx = null;
  let processor = null;
  let chunks = [];       // accumulated Int16 chunks
  let totalLen = 0;      // total Int16 samples accumulated
  let connectedElements = new WeakSet(); // track already-connected media elements

  window.__openhuman_capture = {
    start: async function() {
      if (ctx) return;
      ctx = new (window.AudioContext || window.webkitAudioContext)({ sampleRate: 16000 });

      // ScriptProcessorNode for capturing PCM from page audio elements
      processor = ctx.createScriptProcessor(4096, 1, 1);
      processor.onaudioprocess = function(e) {
        const f32 = e.inputBuffer.getChannelData(0);
        const i16 = new Int16Array(f32.length);
        for (let i = 0; i < f32.length; i++) {
          let s = f32[i];
          s = s < 0 ? s * 0x8000 : s * 0x7FFF;
          s = Math.max(-32768, Math.min(32767, Math.round(s)));
          i16[i] = s;
        }
        chunks.push(i16);
        totalLen += i16.length;
      };

      // Connect the processor to the destination so the pipeline is live
      processor.connect(ctx.destination);

      // Scan for audio/video elements and connect them to the processor.
      // Google Meet plays remote participants' audio through <audio> elements.
      function connectMediaElements() {
        const elements = document.querySelectorAll('audio, video');
        elements.forEach(function(el) {
          if (connectedElements.has(el)) return;
          try {
            // MediaElementAudioSourceNode can only be created once per element
            if (!el._openhuman_source) {
              el._openhuman_source = ctx.createMediaElementSource(el);
            }
            el._openhuman_source.connect(processor);
            connectedElements.add(el);
          } catch(e) {
            // May already be connected or CORS restricted; skip silently
          }
        });
      }

      // Initial scan
      connectMediaElements();

      // Re-scan periodically — Meet may add new audio elements as participants join
      setInterval(connectMediaElements, 2000);
    },

    drain: function() {
      if (totalLen === 0) return "";
      const merged = new Int16Array(totalLen);
      let off = 0;
      for (let i = 0; i < chunks.length; i++) {
        merged.set(chunks[i], off);
        off += chunks[i].length;
      }
      chunks = [];
      totalLen = 0;
      // Convert to base64 efficiently using chunked fromCharCode
      const bytes = new Uint8Array(merged.buffer, merged.byteOffset, merged.byteLength);
      const chunkSize = 8192;
      let binary = "";
      for (let i = 0; i < bytes.length; i += chunkSize) {
        const end = Math.min(i + chunkSize, bytes.length);
        binary += String.fromCharCode.apply(null, bytes.subarray(i, end));
      }
      return btoa(binary);
    }
  };
})()
"#;

/// Injects `window.__openhuman_playback` with `init()`, `feed(b64)`, and
/// `getStream()`.
///
/// `init()` creates an AudioContext at 16 kHz. If `window.__openhuman_fake_audio`
/// exists (set by the fake-camera override), its `MediaStreamDestination` is
/// reused so playback audio feeds into the same stream Meet sees as the mic.
/// Otherwise a new `MediaStreamDestination` is created.
///
/// `feed(b64)` decodes a Base64 string → Int16Array → Float32Array (÷32768)
/// and plays it through a `BufferSourceNode` with precise scheduling (`nextStartTime`)
/// to avoid jitter and popping between consecutive chunks.
///
/// `getStream()` returns the `MediaStream` from the destination for use
/// as a microphone source.
pub(crate) const AUDIO_PLAYBACK_SETUP_JS: &str = r#"
(() => {
  if (window.__openhuman_playback) return;

  let ctx = null;
  let dest = null;
  let nextStartTime = 0;

  window.__openhuman_playback = {
    init: function() {
      if (ctx) return;

      // Reuse the fake-camera's AudioContext + destination if available,
      // so playback audio flows through the same stream Meet consumes.
      if (window.__openhuman_fake_audio) {
        ctx = window.__openhuman_fake_audio.ctx;
        dest = window.__openhuman_fake_audio.dest;
        return;
      }

      ctx = new (window.AudioContext || window.webkitAudioContext)({ sampleRate: 16000 });
      dest = ctx.createMediaStreamDestination();
    },

    feed: function(b64) {
      if (!ctx || !dest) return;
      if (!b64 || b64.length === 0) return;
      try {
        // base64 → binary → Int16Array
        const binary = atob(b64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
          bytes[i] = binary.charCodeAt(i);
        }
        const i16 = new Int16Array(bytes.buffer, bytes.byteOffset, bytes.byteLength / 2);

        // Int16 → Float32
        const f32 = new Float32Array(i16.length);
        for (let i = 0; i < i16.length; i++) {
          f32[i] = i16[i] / 32768.0;
        }

        // Create AudioBuffer and play with precise scheduling
        const buf = ctx.createBuffer(1, f32.length, 16000);
        buf.getChannelData(0).set(f32);
        const src = ctx.createBufferSource();
        src.buffer = buf;
        src.connect(dest);

        // Schedule seamlessly after the previous chunk, or immediately
        // if this is the first chunk or there's a gap.
        const now = ctx.currentTime;
        if (nextStartTime < now) {
          nextStartTime = now;
        }
        src.start(nextStartTime);
        nextStartTime += buf.duration;
      } catch (e) {
        // swallow — playback errors shouldn't crash the bridge
      }
    },

    getStream: function() {
      return dest ? dest.stream : null;
    }
  };
})()
"#;

// ---------------------------------------------------------------------------
// PCM ↔ Base64 helpers
// ---------------------------------------------------------------------------

/// Encode a slice of PCM16LE samples as a Base64 string.
pub fn pcm_to_base64(samples: &[i16]) -> String {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    B64.encode(&bytes)
}

/// Decode a Base64 string into PCM16LE samples.
pub fn base64_to_pcm(b64: &str) -> Result<Vec<i16>, String> {
    if b64.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = B64
        .decode(b64.as_bytes())
        .map_err(|e| format!("base64: {e}"))?;
    if bytes.len() % 2 != 0 {
        return Err(format!("odd byte length {}", bytes.len()));
    }
    Ok(bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect())
}

// ---------------------------------------------------------------------------
// Silence detection
// ---------------------------------------------------------------------------

/// Returns `true` when every sample's absolute value is ≤ `threshold`.
pub fn is_silence(samples: &[i16], threshold: i16) -> bool {
    samples.iter().all(|&s| s.abs() <= threshold)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Inject the capture and playback JS helper objects into the target page.
///
/// Must be called once before `spawn_audio_bridge` starts polling.
pub async fn inject_audio_scripts(
    cdp: &mut CdpConn,
    session_id: &str,
) -> Result<(), String> {
    for (label, script) in [
        ("capture", AUDIO_CAPTURE_SETUP_JS),
        ("playback", AUDIO_PLAYBACK_SETUP_JS),
    ] {
        let res = cdp
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": script,
                    "returnByValue": true,
                    "awaitPromise": false,
                }),
                Some(session_id),
            )
            .await
            .map_err(|e| format!("{LOG_PREFIX} inject {label}: {e}"))?;

        // Check for JS exceptions
        if let Some(exception) = res.get("exceptionDetails") {
            return Err(format!(
                "{LOG_PREFIX} inject {label} JS exception: {exception}"
            ));
        }
    }

    // Start the capture pipeline (getUserMedia)
    cdp.call(
        "Runtime.evaluate",
        json!({
            "expression": "window.__openhuman_capture.start()",
            "returnByValue": true,
            "awaitPromise": true,
        }),
        Some(session_id),
    )
    .await
    .map_err(|e| format!("{LOG_PREFIX} capture.start(): {e}"))?;

    // Initialise the playback pipeline
    cdp.call(
        "Runtime.evaluate",
        json!({
            "expression": "window.__openhuman_playback.init()",
            "returnByValue": true,
            "awaitPromise": false,
        }),
        Some(session_id),
    )
    .await
    .map_err(|e| format!("{LOG_PREFIX} playback.init(): {e}"))?;

    log::info!("{LOG_PREFIX} audio scripts injected and pipelines started");
    Ok(())
}

/// Spawn the bidirectional audio bridge polling loop.
///
/// Returns a [`oneshot::Sender`] — send `()` (or simply drop) to shut down
/// the bridge task.
pub async fn spawn_audio_bridge(
    request_id: String,
    cdp: CdpConn,
    session_id: String,
) -> oneshot::Sender<()> {
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tick.tick().await; // burn first immediate tick

        let mut cdp = cdp;
        let mut errors: u32 = 0;

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    log::info!(
                        "{LOG_PREFIX} bridge shutdown request_id={request_id}"
                    );
                    break;
                }
                _ = tick.tick() => {
                    match audio_tick(&mut cdp, &session_id, &request_id).await {
                        Ok(_) => {
                            errors = 0;
                        }
                        Err(err) => {
                            errors += 1;
                            log::debug!(
                                "{LOG_PREFIX} tick err request_id={request_id} \
                                 consec={errors} err={err}"
                            );
                            if errors >= MAX_CONSECUTIVE_ERRORS {
                                log::warn!(
                                    "{LOG_PREFIX} giving up after {errors} \
                                     consecutive errors request_id={request_id}"
                                );
                                break;
                            }
                            // Exponential backoff: 500ms, 1s, 2s, 4s, 8s, 16s (cap)
                            let shift = (errors - 1).min(5);
                            let backoff = Duration::from_millis(
                                500u64.saturating_mul(1u64 << shift),
                            );
                            tokio::select! {
                                _ = &mut shutdown_rx => {
                                    log::info!(
                                        "{LOG_PREFIX} bridge shutdown during \
                                         backoff request_id={request_id}"
                                    );
                                    break;
                                }
                                _ = tokio::time::sleep(backoff) => {}
                            }
                        }
                    }
                }
            }
        }
    });

    shutdown_tx
}

// ---------------------------------------------------------------------------
// Internal tick implementation
// ---------------------------------------------------------------------------

/// Run one polling cycle: capture inbound audio, push outbound audio.
async fn audio_tick(
    cdp: &mut CdpConn,
    session_id: &str,
    request_id: &str,
) -> Result<(), String> {
    // ── Capture (Meet → Core) ────────────────────────────────────────
    let capture_b64 = cdp_evaluate(cdp, session_id, "window.__openhuman_capture.drain()").await?;

    if !capture_b64.is_empty() {
        let samples = base64_to_pcm(&capture_b64)?;

        if !samples.is_empty() {
            let reg = crate::openhuman::meet_agent::session::registry();
            let vad_event = reg.with_session(request_id, |s| {
                s.push_inbound_pcm(&samples)
            });

            match vad_event {
                Ok(ev) => {
                    use crate::openhuman::meet_agent::ops::VadEvent;
                    if matches!(ev, VadEvent::EndOfUtterance) {
                        let rid = request_id.to_string();
                        tokio::spawn(async move {
                            if let Err(err) =
                                crate::openhuman::meet_agent::brain::run_turn(&rid).await
                            {
                                log::warn!(
                                    "{LOG_PREFIX} brain turn failed \
                                     request_id={rid} err={err}"
                                );
                            }
                        });
                    }
                }
                Err(err) => {
                    log::debug!(
                        "{LOG_PREFIX} push_inbound_pcm failed \
                         request_id={request_id} err={err}"
                    );
                }
            }
        }
    }

    // ── Playback (Core → Meet) ───────────────────────────────────────
    let reg = crate::openhuman::meet_agent::session::registry();
    let outbound = reg.with_session(request_id, |s| s.poll_outbound());

    if let Ok((pcm_b64, _utterance_done)) = outbound {
        if !pcm_b64.is_empty() {
            // Feed the base64 PCM into the page's playback helper.
            let feed_expr = format!(
                "window.__openhuman_playback.feed({})",
                serde_json::to_string(&pcm_b64)
                    .map_err(|e| format!("quote b64: {e}"))?
            );
            cdp_evaluate(cdp, session_id, &feed_expr).await?;
        }
    }

    Ok(())
}

/// Evaluate a JS expression via CDP `Runtime.evaluate` and return the
/// string value (or empty string).
async fn cdp_evaluate(
    cdp: &mut CdpConn,
    session_id: &str,
    expression: &str,
) -> Result<String, String> {
    let res = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": false,
            }),
            Some(session_id),
        )
        .await?;

    if let Some(exception) = res.get("exceptionDetails") {
        return Err(format!("JS exception: {exception}"));
    }

    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(value)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_to_base64_roundtrip() {
        let original: Vec<i16> = vec![0, 1000, -2000, 32767, -32768, 123];
        let b64 = pcm_to_base64(&original);
        let decoded = base64_to_pcm(&b64).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn base64_to_pcm_empty_string() {
        let result = base64_to_pcm("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn base64_to_pcm_odd_length_rejected() {
        // "AA" decodes to a single byte — odd byte count for i16
        let one_byte_b64 = B64.encode([0u8]);
        let err = base64_to_pcm(&one_byte_b64).unwrap_err();
        assert!(err.contains("odd byte length"));
    }

    #[test]
    fn is_silence_with_all_zeros() {
        let samples: Vec<i16> = vec![0, 0, 0, 0];
        assert!(is_silence(&samples, 0));
    }

    #[test]
    fn is_silence_with_loud_sample() {
        let samples: Vec<i16> = vec![0, 0, 5000, 0];
        assert!(!is_silence(&samples, 100));
    }

    #[test]
    fn samples_per_tick_sane() {
        assert_eq!(SAMPLES_PER_TICK, 1600);
    }
}
