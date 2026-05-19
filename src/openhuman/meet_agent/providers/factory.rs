//! Provider factory — creates provider instances from config.
//!
//! Default: all tinyhumans (backward compatible).
//! Reads `[meet_agent]` TOML section for provider selection.

use super::noop::{NoopLlm, NoopStt, NoopTts};
use super::tinyhumans::{TinyhumansLlm, TinyhumansStt, TinyhumansTts};
use super::{MeetingLLM, SpeechToText, TextToSpeech};

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
pub fn create_providers_from_config(
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
