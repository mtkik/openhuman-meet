//! Turn orchestration: STT → LLM → TTS.
//!
//! ## Pipeline
//!
//! When [`session::Vad`] reports `EndOfUtterance`, [`run_turn`] drains
//! the inbound buffer and runs three serial stages:
//!
//! 1. **STT** — wrap the PCM16LE samples in a WAV container and post
//!    to the configured STT provider (default: tinyhumans backend via
//!    [`crate::openhuman::voice::cloud_transcribe`]). Returns the
//!    transcribed text (or `Err` on transport / auth failure).
//!
//! 2. **LLM** — send a tiny chat-completions request to the configured
//!    LLM provider (default: tinyhumans backend via
//!    [`crate::api::BackendOAuthClient`]) with a "live meeting agent"
//!    system prompt and the transcript as the user message. Returns a
//!    short reply (or empty string when the agent decides to stay
//!    silent).
//!
//! 3. **TTS** — feed the reply text to the configured TTS provider
//!    (default: tinyhumans backend via
//!    [`crate::openhuman::voice::reply_speech`]) requesting
//!    `output_format = "pcm_16000"`. Decode the base64 PCM bytes back
//!    into `Vec<i16>` and enqueue on the session's outbound queue.
//!
//! ## Provider Trait Architecture
//!
//! Providers implement [`super::providers::SpeechToText`],
//! [`super::providers::MeetingLLM`], and [`super::providers::TextToSpeech`].
//! The default set (tinyhumans backend) preserves exact backward
//! compatibility. Use [`super::providers::factory::create_providers_from_config`]
//! to select providers from the `[meet.meet_agent]` config section.
//!
//! ## Fallback
//!
//! When the backend session token is missing (the most common reason
//! a stage fails outside production: tests, no-network smoke runs),
//! we fall back to deterministic stubs so the loop still produces an
//! audible blip and the unit tests stay network-free. Real
//! transport / 5xx errors are *not* swallowed — they surface as
//! `Note` events so a real-call failure is visible in the transcript
//! log, not silently degraded to a stub.

use serde_json::{json, Value};

use super::providers::factory;
use super::providers::{MeetingLLM, SpeechToText, TextToSpeech};
use super::session::registry;
use super::types::{SessionEvent, SessionEventKind};

/// How many of the most recent `Heard` / `Spoke` events we feed back
/// into the LLM as rolling conversation context. 12 ≈ a few minutes of
/// captioned dialogue — enough for the model to follow a thread without
/// blowing the prompt budget.
const CONTEXT_EVENT_WINDOW: usize = 12;
/// Spoken-reply ceiling. Each token is roughly ¾ of a word, so 220
/// tokens ≈ 30 seconds of speech — long enough for a real answer, short
/// enough that the model can't hijack the meeting.
const REPLY_MAX_TOKENS: u32 = 220;

/// Minimum samples below which we skip the brain turn entirely.
/// 250 ms @ 16 kHz — under this, VAD almost certainly fired on a
/// transient (cough, click) rather than real speech.
const MIN_TURN_SAMPLES: usize = 4_000;
/// Re-exported from `ops` so any drift (if we ever loosen the
/// boundary check) immediately breaks the WAV / duration math here
/// at compile time. Today the same constant is used in both places —
/// the ops boundary check rejects anything else outright.
const SAMPLE_RATE_HZ: u32 = super::ops::REQUIRED_SAMPLE_RATE;

/// Caption-driven turn. Drains the session's pending wake-word prompt
/// (assembled by `session::note_caption`) and runs LLM → TTS → enqueue
/// outbound. Skips STT entirely — the captions are already text.
///
/// We give the user a short window (`CAPTION_TURN_DELAY_MS`) after the
/// wake word fires so multi-caption utterances (\"hey openhuman …
/// what's the weather like in paris\") have a chance to assemble
/// before we hit the LLM. The shell calls this on every caption
/// push that flagged the wake word; subsequent calls before the
/// delay expires are coalesced via the session's `wake_active` flag.
///
/// This overload loads the configured providers (via `load_providers`),
/// falling back to tinyhumans when no config is available.
pub async fn run_caption_turn(request_id: &str) -> Result<bool, String> {
    let (stt, llm, tts) = load_providers().await;
    run_caption_turn_with_providers(request_id, &*stt, &*llm, &*tts).await
}

/// Caption-driven turn with explicit providers. See [`run_caption_turn`].
pub async fn run_caption_turn_with_providers(
    request_id: &str,
    _stt: &dyn SpeechToText,
    llm: &dyn MeetingLLM,
    tts: &dyn TextToSpeech,
) -> Result<bool, String> {
    // Wait briefly so a multi-fragment wake utterance ("hey openhuman
    // what's the weather like in paris" arriving as 2-3 captions) has
    // a chance to assemble before we drain the prompt.
    tokio::time::sleep(std::time::Duration::from_millis(CAPTION_TURN_DELAY_MS)).await;

    let (prompt, history) = match registry().with_session(request_id, |s| {
        let prompt = s.take_pending_prompt();
        let history = recent_dialog_history(s.events(), CONTEXT_EVENT_WINDOW);
        (prompt, history)
    })? {
        (Some(p), h) => (p, h),
        (None, _) => return Ok(false),
    };
    log::info!(
        "[meet-agent] caption turn start request_id={request_id} prompt_chars={} history_msgs={}",
        prompt.chars().count(),
        history.len(),
    );

    // Real LLM call. The model gets the rolling caption history plus
    // the user's direct address and decides whether to respond, what
    // to say, and how concise to be. It can also return an empty
    // string when it concludes the message wasn't actually directed
    // at it (false-positive wake word, side conversation).
    let reply_text = match llm
        .reply(&prompt, &history, MEETING_SYSTEM_PROMPT, REPLY_MAX_TOKENS)
        .await
    {
        Ok(text) => text,
        Err(err) => {
            log::warn!("[meet-agent] caption-turn LLM failed request_id={request_id} err={err}");
            let _ = registry().with_session(request_id, |s| {
                s.record_event(
                    SessionEventKind::Note,
                    format!("LLM failure (using ack): {err}"),
                );
            });
            pick_ack_phrase(&prompt).to_string()
        }
    };

    let synthesized = if reply_text.trim().is_empty() {
        Vec::new()
    } else {
        match tts.synthesize(&reply_text, SAMPLE_RATE_HZ).await {
            Ok(samples) => samples,
            Err(err) => {
                log::warn!(
                    "[meet-agent] caption-turn TTS failed request_id={request_id} err={err}"
                );
                let _ = registry().with_session(request_id, |s| {
                    s.record_event(
                        SessionEventKind::Note,
                        format!("TTS failure (using stub): {err}"),
                    );
                });
                stub_tts(&reply_text).await
            }
        }
    };

    registry().with_session(request_id, |s| {
        s.record_event(SessionEventKind::Heard, prompt.clone());
        if !reply_text.is_empty() {
            s.record_event(SessionEventKind::Spoke, reply_text.clone());
            if !synthesized.is_empty() {
                s.enqueue_outbound_pcm(&synthesized, true);
            }
        } else {
            s.record_event(
                SessionEventKind::Note,
                "agent declined to respond".to_string(),
            );
        }
        s.turn_count += 1;
    })?;

    log::info!(
        "[meet-agent] caption turn done request_id={request_id} reply_chars={} synth_samples={}",
        reply_text.chars().count(),
        synthesized.len()
    );
    Ok(true)
}

/// Delay between wake-word match and prompt drain. Long enough that
/// 2-3 caption fragments can join up; short enough that the user
/// doesn't experience awkward silence after they stop talking.
const CAPTION_TURN_DELAY_MS: u64 = 1_500;

/// Canned acknowledgements the agent speaks out loud after capturing
/// a note. Short, varied so consecutive notes don't sound robotic.
/// Selected by hashing the prompt so the same dictation reliably
/// produces the same ack (helpful for tests + debugging) while still
/// rotating across the set in a normal conversation.
const ACK_PHRASES: &[&str] = &["Got it.", "Noted.", "Adding that.", "On it.", "Captured."];

fn pick_ack_phrase(prompt: &str) -> &'static str {
    if prompt.trim().is_empty() {
        return "";
    }
    let h: u32 = prompt.bytes().fold(0u32, |a, b| a.wrapping_add(b as u32));
    ACK_PHRASES[(h as usize) % ACK_PHRASES.len()]
}

/// Fire one brain turn for the named session. Returns `Ok(true)` when a
/// turn actually ran, `Ok(false)` when the inbound buffer was below the
/// floor.
///
/// This overload loads the configured providers (via `load_providers`),
/// falling back to tinyhumans when no config is available.
pub async fn run_turn(request_id: &str) -> Result<bool, String> {
    let (stt, llm, tts) = load_providers().await;
    run_turn_with_providers(request_id, &*stt, &*llm, &*tts).await
}

/// Load providers from the on-disk config when possible. Falls back to
/// the default (tinyhumans) provider set when config loading fails — that
/// keeps the test / no-network path working without requiring an
/// `~/.openhuman/config.toml` to exist.
async fn load_providers() -> (
    Box<dyn SpeechToText>,
    Box<dyn MeetingLLM>,
    Box<dyn TextToSpeech>,
) {
    match crate::openhuman::config::ops::load_config_with_timeout().await {
        Ok(config) => factory::create_providers_from_config(&config.meet.meet_agent),
        Err(err) => {
            log::debug!(
                "[meet-agent] config load failed ({err}), using default providers"
            );
            factory::create_default_providers()
        }
    }
}

/// Fire one brain turn with explicit providers. See [`run_turn`].
pub async fn run_turn_with_providers(
    request_id: &str,
    stt: &dyn SpeechToText,
    llm: &dyn MeetingLLM,
    tts: &dyn TextToSpeech,
) -> Result<bool, String> {
    let (drained, history) = registry().with_session(request_id, |s| {
        let drained = s.drain_inbound();
        let history = recent_dialog_history(s.events(), CONTEXT_EVENT_WINDOW);
        (drained, history)
    })?;
    if drained.len() < MIN_TURN_SAMPLES {
        log::debug!(
            "[meet-agent] skipping turn request_id={request_id} samples={}",
            drained.len()
        );
        return Ok(false);
    }

    log::info!(
        "[meet-agent] turn start request_id={request_id} samples={}",
        drained.len()
    );

    // ─── STT ────────────────────────────────────────────────────────
    let heard = match stt.transcribe(&drained, SAMPLE_RATE_HZ).await {
        Ok(text) if text.trim().is_empty() => {
            log::info!("[meet-agent] STT empty, skipping turn request_id={request_id}");
            return Ok(false);
        }
        Ok(text) => text,
        Err(err) => {
            log::warn!("[meet-agent] STT failed request_id={request_id} err={err}");
            // Record a Note so the transcript log makes the failure
            // visible to whoever's looking at logs.
            let _ = registry().with_session(request_id, |s| {
                s.record_event(
                    SessionEventKind::Note,
                    format!("STT failure (using stub): {err}"),
                );
            });
            stub_stt(&drained).await
        }
    };
    log::info!(
        "[meet-agent] STT request_id={request_id} text_chars={}",
        heard.chars().count()
    );

    // ─── LLM ────────────────────────────────────────────────────────
    let reply_text = match llm
        .reply(&heard, &history, MEETING_SYSTEM_PROMPT, REPLY_MAX_TOKENS)
        .await
    {
        Ok(text) => text,
        Err(err) => {
            log::warn!("[meet-agent] LLM failed request_id={request_id} err={err}");
            let _ = registry().with_session(request_id, |s| {
                s.record_event(
                    SessionEventKind::Note,
                    format!("LLM failure (using stub): {err}"),
                );
            });
            stub_llm(&heard).await
        }
    };

    // ─── TTS ────────────────────────────────────────────────────────
    let synthesized = if reply_text.trim().is_empty() {
        Vec::new()
    } else {
        match tts.synthesize(&reply_text, SAMPLE_RATE_HZ).await {
            Ok(samples) => samples,
            Err(err) => {
                log::warn!("[meet-agent] TTS failed request_id={request_id} err={err}");
                let _ = registry().with_session(request_id, |s| {
                    s.record_event(
                        SessionEventKind::Note,
                        format!("TTS failure (using stub): {err}"),
                    );
                });
                stub_tts(&reply_text).await
            }
        }
    };

    registry().with_session(request_id, |s| {
        s.record_event(SessionEventKind::Heard, heard.clone());
        if !reply_text.is_empty() {
            s.record_event(SessionEventKind::Spoke, reply_text.clone());
            if !synthesized.is_empty() {
                s.enqueue_outbound_pcm(&synthesized, true);
            }
        } else {
            s.record_event(
                SessionEventKind::Note,
                "agent declined to respond".to_string(),
            );
        }
        s.turn_count += 1;
    })?;

    log::info!(
        "[meet-agent] turn done request_id={request_id} reply_chars={} synth_samples={}",
        reply_text.chars().count(),
        synthesized.len()
    );
    Ok(true)
}

// ─── System prompt ──────────────────────────────────────────────────

/// System prompt for the live meeting agent. Pushes the model toward
/// (a) recognising whether the latest utterance is genuinely directed
/// at it (intent classification — emit empty string when not), and
/// (b) responding conversationally and concisely when it is.
const MEETING_SYSTEM_PROMPT: &str = "\
You are OpenHuman, an AI assistant joining a live Google Meet call as a participant. \
The meeting transcript is provided as prior turns where `user` lines are captions \
spoken by humans on the call (sometimes prefixed with their name) and `assistant` \
lines are things you previously said out loud. The latest `user` message is the \
utterance you are deciding how to respond to.\n\
\n\
Decide first: was this latest utterance actually directed at you? Strong signals: \
the speaker addresses you by name (\"OpenHuman\", \"hey openhuman\"), asks a direct \
question, or asks you to do something (note this, summarise, look up, remember, \
remind, draft). Weak signals (do NOT respond): chit-chat between humans, \
side conversation, your name appearing inside a longer thought aimed at someone \
else, ambient transcription noise.\n\
\n\
If it is NOT directed at you, output exactly the empty string. Stay silent. \n\
\n\
If it IS directed at you:\n\
  • Reply in 1–2 spoken sentences. Conversational, warm, direct. No filler.\n\
  • Pronounce naturally — write the way a person speaks, not the way they type. \
No markdown, no bullet lists, no code blocks, no emoji.\n\
  • For dictation / note requests (\"remember…\", \"action item…\", \"follow up on…\"), \
the note is already captured in the transcript log, so just acknowledge briefly \
(\"Got it.\", \"Adding that.\") — don't read the note back.\n\
  • For questions, answer directly with what you know; if you don't know, say so \
in one sentence rather than guessing.\n\
  • Never repeat verbatim what was said. Never describe what you're about to do — \
just do it.\n\
";

// ─── History helpers ────────────────────────────────────────────────

use super::providers::ConversationTurn;

/// Pull the last `window` `Heard`/`Spoke` events from the session log
/// and shape them into chat-completions turns. `Note` events are
/// internal book-keeping (errors, wake-word matches) and are skipped.
fn recent_dialog_history(events: &[SessionEvent], window: usize) -> Vec<ConversationTurn> {
    let mut out: Vec<ConversationTurn> = Vec::with_capacity(window);
    for e in events.iter().rev() {
        if out.len() >= window {
            break;
        }
        let role = match e.kind {
            SessionEventKind::Heard => "user",
            SessionEventKind::Spoke => "assistant",
            SessionEventKind::Note => continue,
        };
        let content = e.text.trim();
        if content.is_empty() {
            continue;
        }
        out.push(ConversationTurn {
            role: role.to_string(),
            content: content.to_string(),
        });
    }
    out.reverse();
    out
}

// ─── Stubs (fallback for tests / no-backend) ────────────────────────

async fn stub_stt(samples: &[i16]) -> String {
    let secs = samples.len() as f32 / SAMPLE_RATE_HZ as f32;
    format!("(heard ~{secs:.1}s of audio)")
}

async fn stub_llm(_heard: &str) -> String {
    "I'm listening.".to_string()
}

async fn stub_tts(text: &str) -> Vec<i16> {
    if text.is_empty() {
        return Vec::new();
    }
    let sample_rate = SAMPLE_RATE_HZ as f32;
    let freq = 440.0_f32;
    let duration_secs = 0.2_f32;
    let count = (sample_rate * duration_secs) as usize;
    (0..count)
        .map(|i| {
            let t = i as f32 / sample_rate;
            (((2.0 * std::f32::consts::PI * freq * t).sin()) * (i16::MAX as f32 * 0.3)) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openhuman::meet_agent::session::registry;

    #[tokio::test]
    async fn run_turn_skips_short_buffers() {
        registry().start("brain-skip", 16_000).unwrap();
        registry()
            .with_session("brain-skip", |s| {
                s.push_inbound_pcm(&vec![0; 800]); // 50ms — under floor
            })
            .unwrap();
        assert_eq!(run_turn("brain-skip").await.unwrap(), false);
        let _ = registry().stop("brain-skip");
    }

    #[tokio::test]
    async fn run_turn_falls_back_to_stub_without_backend() {
        // No backend session in test env → STT/LLM/TTS all fail and
        // each stage falls back to its stub. The turn still produces
        // a Heard event, a Spoke event, and synthesized PCM, so the
        // smoke-test contract holds.
        registry().start("brain-fallback", 16_000).unwrap();
        registry()
            .with_session("brain-fallback", |s| {
                s.push_inbound_pcm(&vec![1000; 16_000]); // 1s
            })
            .unwrap();
        assert_eq!(run_turn("brain-fallback").await.unwrap(), true);
        registry()
            .with_session("brain-fallback", |s| {
                let kinds: Vec<_> = s.events().iter().map(|e| format!("{:?}", e.kind)).collect();
                assert!(kinds.contains(&"Heard".to_string()));
                assert!(kinds.contains(&"Spoke".to_string()));
                assert_eq!(s.turn_count, 1);
                assert!(s.spoken_seconds() > 0.0);
            })
            .unwrap();
        let _ = registry().stop("brain-fallback");
    }

    #[test]
    fn extract_chat_completion_text_pulls_first_choice() {
        let raw = json!({
            "choices": [
                { "message": { "content": "  hello world  " } }
            ]
        });
        assert_eq!(
            super::super::providers::extract_chat_completion_text(&raw),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn extract_chat_completion_text_returns_none_on_malformed() {
        assert_eq!(
            super::super::providers::extract_chat_completion_text(&json!({})),
            None
        );
        assert_eq!(
            super::super::providers::extract_chat_completion_text(&json!({ "choices": [] })),
            None
        );
    }

    #[test]
    fn recent_dialog_history_maps_event_kinds_to_chat_roles() {
        let now = 0;
        let events = vec![
            SessionEvent {
                kind: SessionEventKind::Heard,
                text: "Alice: how's the build going".into(),
                timestamp_ms: now,
            },
            SessionEvent {
                kind: SessionEventKind::Note,
                text: "wake word".into(),
                timestamp_ms: now,
            },
            SessionEvent {
                kind: SessionEventKind::Spoke,
                text: "Build is green.".into(),
                timestamp_ms: now,
            },
            SessionEvent {
                kind: SessionEventKind::Heard,
                text: "Bob: ship it".into(),
                timestamp_ms: now,
            },
        ];
        let history = recent_dialog_history(&events, 10);
        assert_eq!(history.len(), 3, "Note events are dropped");
        assert_eq!(history[0].role, "user");
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[2].role, "user");
        assert_eq!(history[2].content, "Bob: ship it");
    }

    #[test]
    fn recent_dialog_history_caps_at_window_keeping_most_recent() {
        let events: Vec<SessionEvent> = (0..30)
            .map(|i| SessionEvent {
                kind: SessionEventKind::Heard,
                text: format!("line {i}"),
                timestamp_ms: 0,
            })
            .collect();
        let history = recent_dialog_history(&events, 5);
        assert_eq!(history.len(), 5);
        assert_eq!(history[0].content, "line 25");
        assert_eq!(history[4].content, "line 29");
    }

    #[test]
    fn strip_for_speech_removes_markdown_punctuation_and_fences() {
        use super::super::providers::tinyhumans::strip_for_speech;
        let raw = "**Got it.** Adding `that` to your follow-ups.";
        assert_eq!(
            strip_for_speech(raw),
            "Got it. Adding that to your follow-ups."
        );
        let fenced = "Sure:\n```\ncode\n```\nDone.";
        assert_eq!(strip_for_speech(fenced), "Sure: Done.");
        let bullets = "- one\n- two";
        assert_eq!(strip_for_speech(bullets), "one two");
    }

    #[test]
    fn strip_for_speech_preserves_empty_when_input_empty() {
        use super::super::providers::tinyhumans::strip_for_speech;
        assert_eq!(strip_for_speech(""), "");
        assert_eq!(strip_for_speech("   \n  "), "");
    }
}
