//! Provider factory — creates provider instances from config.
//!
//! Default: all tinyhumans (backward compatible).
//! Reads `[meet_agent]` TOML section for provider selection.

use super::gemini_llm::GeminiLlm;
use super::google_stt::GoogleStt;
use super::google_tts::GoogleTts;
use super::noop::{NoopLlm, NoopStt, NoopTts};
use super::tinyhumans::{TinyhumansLlm, TinyhumansStt, TinyhumansTts};
use super::{MeetingLLM, SpeechToText, TextToSpeech};
use crate::openhuman::config::schema::MeetAgentConfig;

/// Create the default set of providers (tinyhumans backend).
///
/// This is the backward-compatible default — all providers use the
/// original tinyhumans/OpenHuman backend.
pub fn create_default_providers(
) -> (Box<dyn SpeechToText>, Box<dyn MeetingLLM>, Box<dyn TextToSpeech>) {
    (
        Box::new(TinyhumansStt),
        Box::new(TinyhumansLlm),
        Box::new(TinyhumansTts),
    )
}

/// Create no-op providers for testing.
pub fn create_noop_providers(
) -> (Box<dyn SpeechToText>, Box<dyn MeetingLLM>, Box<dyn TextToSpeech>) {
    (
        Box::new(NoopStt),
        Box::new(NoopLlm),
        Box::new(NoopTts),
    )
}

/// Create providers based on the config's `meet_agent` section.
///
/// If no `meet_agent` section is present or if provider fields are
/// missing, falls back to tinyhumans defaults.
///
/// The `config` parameter supplies provider-specific settings (API keys,
/// model names, etc.). Provider names are matched case-sensitively:
/// - `"noop"` — no-op stubs
/// - `"google"` — Google Cloud STT / TTS
/// - `"gemini"` — Gemini via OpenAI-compatible endpoint
/// - `"tinyhumans"` (or any unknown) — original backend
pub fn create_providers_from_config(
    config: &MeetAgentConfig,
) -> (Box<dyn SpeechToText>, Box<dyn MeetingLLM>, Box<dyn TextToSpeech>) {
    let stt: Box<dyn SpeechToText> = match config.stt_provider.as_str() {
        "noop" => Box::new(NoopStt),
        "google" => match GoogleStt::from_config(config) {
            Ok(p) => Box::new(p),
            Err(e) => {
                tracing::warn!("google STT init failed ({e}), falling back to tinyhumans");
                Box::new(TinyhumansStt)
            }
        },
        // "tinyhumans" or any unknown → default
        _ => Box::new(TinyhumansStt),
    };

    let llm: Box<dyn MeetingLLM> = match config.llm_provider.as_str() {
        "noop" => Box::new(NoopLlm),
        "gemini" => match GeminiLlm::from_config(config) {
            Ok(p) => Box::new(p),
            Err(e) => {
                tracing::warn!("gemini LLM init failed ({e}), falling back to tinyhumans");
                Box::new(TinyhumansLlm)
            }
        },
        _ => Box::new(TinyhumansLlm),
    };

    let tts: Box<dyn TextToSpeech> = match config.tts_provider.as_str() {
        "noop" => Box::new(NoopTts),
        "google" => match GoogleTts::from_config(config) {
            Ok(p) => Box::new(p),
            Err(e) => {
                tracing::warn!("google TTS init failed ({e}), falling back to tinyhumans");
                Box::new(TinyhumansTts)
            }
        },
        _ => Box::new(TinyhumansTts),
    };

    (stt, llm, tts)
}

/// Legacy entry point — creates providers by name without config.
///
/// Used by callers that only know the provider name string but don't
/// have a full `MeetAgentConfig`. Self-hosted providers ("google",
/// "gemini") will fall back to tinyhumans because they need config
/// for API keys.
pub fn create_providers_by_name(
    stt_name: Option<&str>,
    llm_name: Option<&str>,
    tts_name: Option<&str>,
) -> (Box<dyn SpeechToText>, Box<dyn MeetingLLM>, Box<dyn TextToSpeech>) {
    let stt: Box<dyn SpeechToText> = match stt_name.unwrap_or("tinyhumans") {
        "noop" => Box::new(NoopStt),
        // "tinyhumans" or any unknown → default
        _ => Box::new(TinyhumansStt),
    };

    let llm: Box<dyn MeetingLLM> = match llm_name.unwrap_or("tinyhumans") {
        "noop" => Box::new(NoopLlm),
        _ => Box::new(TinyhumansLlm),
    };

    let tts: Box<dyn TextToSpeech> = match tts_name.unwrap_or("tinyhumans") {
        "noop" => Box::new(NoopTts),
        _ => Box::new(TinyhumansTts),
    };

    (stt, llm, tts)
}
