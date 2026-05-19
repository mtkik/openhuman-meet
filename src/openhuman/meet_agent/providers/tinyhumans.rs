//! Tinyhumans backend provider implementations.
//!
//! Wraps the existing `cloud_transcribe`, `BackendOAuthClient`, and
//! `reply_speech` calls from the original brain.rs. This is the default
//! provider for backward compatibility.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};

use super::{
    extract_chat_completion_text, ConversationTurn, MeetingLLM, SpeechToText, TextToSpeech,
};
use crate::openhuman::meet_agent::wav;

// ─── STT ─────────────────────────────────────────────────────────────

/// Tinyhumans STT provider — wraps `cloud_transcribe::transcribe_cloud`.
pub struct TinyhumansStt;

#[async_trait]
impl SpeechToText for TinyhumansStt {
    async fn transcribe(&self, pcm: &[i16], sample_rate: u32) -> Result<String, String> {
        use crate::openhuman::voice::cloud_transcribe::{transcribe_cloud, CloudTranscribeOptions};

        let config = crate::openhuman::config::ops::load_config_with_timeout().await?;
        let wav_bytes = wav::pack_pcm16le_mono_wav(pcm, sample_rate);
        let audio_b64 = B64.encode(&wav_bytes);
        let opts = CloudTranscribeOptions {
            mime_type: Some("audio/wav".to_string()),
            file_name: Some("meet-agent.wav".to_string()),
            ..Default::default()
        };
        let outcome = transcribe_cloud(&config, &audio_b64, &opts).await?;
        Ok(outcome.value.text.clone())
    }
}

// ─── LLM ─────────────────────────────────────────────────────────────

/// Tinyhumans LLM provider — wraps `BackendOAuthClient` + chat completions.
pub struct TinyhumansLlm;

#[async_trait]
impl MeetingLLM for TinyhumansLlm {
    async fn reply(
        &self,
        prompt: &str,
        history: &[ConversationTurn],
        system_prompt: &str,
        max_tokens: u32,
    ) -> Result<String, String> {
        use crate::api::config::effective_backend_api_url;
        use crate::api::jwt::get_session_token;
        use crate::api::BackendOAuthClient;
        use reqwest::Method;

        let config = crate::openhuman::config::ops::load_config_with_timeout().await?;
        let token = get_session_token(&config)
            .map_err(|e| e.to_string())?
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| "no backend session token".to_string())?;

        let api_url = effective_backend_api_url(&config.api_url);
        let client = BackendOAuthClient::new(&api_url).map_err(|e| e.to_string())?;

        let mut messages: Vec<Value> = Vec::with_capacity(history.len() + 2);
        messages.push(json!({ "role": "system", "content": system_prompt }));
        for turn in history {
            messages.push(json!({ "role": turn.role, "content": turn.content }));
        }
        messages.push(json!({ "role": "user", "content": prompt }));

        let body = json!({
            "model": "agentic-v1",
            "temperature": 0.5,
            "max_tokens": max_tokens,
            "messages": messages,
        });

        let raw = client
            .authed_json(
                &token,
                Method::POST,
                "/openai/v1/chat/completions",
                Some(body),
            )
            .await
            .map_err(|e| e.to_string())?;

        let text = extract_chat_completion_text(&raw)
            .ok_or_else(|| format!("unexpected chat completions response: {raw}"))?;
        Ok(strip_for_speech(&text))
    }
}

// ─── TTS ─────────────────────────────────────────────────────────────

/// ElevenLabs model identifier used by the tinyhumans backend.
const TTS_MODEL_ID: &str = "eleven_turbo_v2_5";

/// Tinyhumans TTS provider — wraps `reply_speech::synthesize_reply`.
pub struct TinyhumansTts;

#[async_trait]
impl TextToSpeech for TinyhumansTts {
    async fn synthesize(&self, text: &str, _sample_rate: u32) -> Result<Vec<i16>, String> {
        use crate::openhuman::voice::reply_speech::{synthesize_reply, ReplySpeechOptions};

        let config = crate::openhuman::config::ops::load_config_with_timeout().await?;
        let voice_settings = json!({
            "stability": 0.4,
            "similarity_boost": 0.75,
            "style": 0.35,
            "use_speaker_boost": true,
        });
        let opts = ReplySpeechOptions {
            output_format: Some("pcm_16000".to_string()),
            model_id: Some(TTS_MODEL_ID.to_string()),
            voice_settings: Some(voice_settings),
            ..Default::default()
        };
        let outcome = synthesize_reply(&config, text, &opts).await?;
        let result = outcome.value;
        let pcm_bytes = B64
            .decode(result.audio_base64.as_bytes())
            .map_err(|e| format!("decode tts base64: {e}"))?;
        if !pcm_bytes.len().is_multiple_of(2) {
            return Err(format!("odd byte length from tts: {}", pcm_bytes.len()));
        }
        Ok(pcm_bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect())
    }
}

// ─── Utility functions (moved from brain.rs) ─────────────────────────

/// Trim characters that sound bad when read aloud by TTS but routinely
/// leak from a chat-completions response (markdown asterisks, fenced
/// code, leading bullets). Keep punctuation that affects prosody
/// (commas, periods, question marks) intact.
pub fn strip_for_speech(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let cleaned: String = trimmed
            .trim_start_matches(|c: char| c == '-' || c == '*' || c == '#' || c == '>')
            .trim()
            .chars()
            .filter(|c| !matches!(c, '*' | '`' | '_' | '#'))
            .collect();
        if cleaned.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&cleaned);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::super::extract_chat_completion_text;
    use super::*;
    use crate::openhuman::config::TEST_ENV_LOCK;
    use serde_json::json;
    use tempfile::tempdir;

    // ── Pure-function tests ──────────────────────────────────────────

    #[test]
    fn strip_for_speech_removes_markdown() {
        let input = "**bold** `code` and more\n```\nfenced code\n```\n- bullet";
        let out = strip_for_speech(input);
        assert!(!out.contains('*'), "asterisks remain: {out}");
        assert!(!out.contains('`'), "backticks remain: {out}");
        assert!(!out.contains("fenced"), "fenced block not stripped: {out}");
        assert!(out.contains("bold"), "content lost: {out}");
        assert!(out.contains("bullet"), "bullet text lost: {out}");
    }

    #[test]
    fn strip_for_speech_preserves_punctuation() {
        let out = strip_for_speech("Hello, world. How are you?");
        assert_eq!(out, "Hello, world. How are you?");
    }

    #[test]
    fn strip_for_speech_drops_empty_lines() {
        let out = strip_for_speech("first\n\n\nsecond");
        assert_eq!(out, "first second");
    }

    #[test]
    fn extract_chat_completion_text_parses_standard_response() {
        let raw = json!({
            "choices": [{
                "message": { "role": "assistant", "content": "  hello there  " }
            }]
        });
        assert_eq!(
            extract_chat_completion_text(&raw),
            Some("hello there".to_string())
        );
    }

    #[test]
    fn extract_chat_completion_text_returns_none_on_empty() {
        assert_eq!(extract_chat_completion_text(&json!({ "choices": [] })), None);
    }

    #[test]
    fn extract_chat_completion_text_returns_none_on_missing_content() {
        let raw = json!({ "choices": [{ "message": { "role": "assistant" } }] });
        assert_eq!(extract_chat_completion_text(&raw), None);
    }

    #[test]
    fn extract_chat_completion_text_returns_none_on_missing_choices() {
        assert_eq!(extract_chat_completion_text(&json!({})), None);
    }

    // ── No-session-token error path tests ────────────────────────────
    //
    // These exercise the tinyhumans providers when no backend session
    // token is configured. We point OPENHUMAN_WORKSPACE at a fresh
    // tempdir so `load_config_with_timeout()` sees a clean state, then
    // assert each provider returns an Err (rather than panicking or
    // hanging). The TEST_ENV_LOCK serialises env mutation across the
    // crate's test modules.

    #[tokio::test]
    async fn tinyhumans_stt_returns_error_without_token() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir().unwrap();
        std::env::set_var("OPENHUMAN_WORKSPACE", tmp.path());

        let stt = TinyhumansStt;
        let result = stt.transcribe(&vec![0i16; 1600], 16_000).await;

        std::env::remove_var("OPENHUMAN_WORKSPACE");
        assert!(result.is_err(), "stt without token should err");
    }

    #[tokio::test]
    async fn tinyhumans_llm_returns_error_without_token() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir().unwrap();
        std::env::set_var("OPENHUMAN_WORKSPACE", tmp.path());

        let llm = TinyhumansLlm;
        let result = llm.reply("hi", &[], "sys", 100).await;

        std::env::remove_var("OPENHUMAN_WORKSPACE");
        let err = result.expect_err("llm without token should err");
        assert!(
            err.contains("session token") || err.contains("no backend"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn tinyhumans_tts_returns_error_without_token() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir().unwrap();
        std::env::set_var("OPENHUMAN_WORKSPACE", tmp.path());

        let tts = TinyhumansTts;
        let result = tts.synthesize("hello", 16_000).await;

        std::env::remove_var("OPENHUMAN_WORKSPACE");
        assert!(result.is_err(), "tts without token should err");
    }
}
