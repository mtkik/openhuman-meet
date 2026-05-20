//! Provider traits for the Meet Agent pipeline.
//!
//! Three independent traits allow mixing providers:
//! - [`SpeechToText`]: PCM → text
//! - [`MeetingLLM`]: prompt + history → reply text
//! - [`TextToSpeech`]: text → PCM

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Conversation turn for LLM history context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub role: String,
    pub content: String,
}

/// Extract the assistant reply text from an OpenAI-compatible
/// chat-completions JSON response (`choices[0].message.content`).
/// Returns `None` when the response is missing the expected shape so
/// callers can attach their own provider-specific error context.
pub fn extract_chat_completion_text(raw: &Value) -> Option<String> {
    raw.get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|first| first.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|s| s.as_str())
        .map(|s| s.trim().to_string())
}

/// STT provider trait.
#[async_trait]
pub trait SpeechToText: Send + Sync {
    /// Transcribe PCM16LE audio to text.
    async fn transcribe(&self, pcm: &[i16], sample_rate: u32) -> Result<String, String>;
}

/// LLM provider trait for meeting agent replies.
#[async_trait]
pub trait MeetingLLM: Send + Sync {
    /// Generate a reply given the current prompt and conversation history.
    async fn reply(
        &self,
        prompt: &str,
        history: &[ConversationTurn],
        system_prompt: &str,
        max_tokens: u32,
    ) -> Result<String, String>;
}

/// TTS provider trait.
#[async_trait]
pub trait TextToSpeech: Send + Sync {
    /// Synthesize text to PCM16LE audio at the given sample rate.
    async fn synthesize(&self, text: &str, sample_rate: u32) -> Result<Vec<i16>, String>;
}

pub mod factory;
pub mod gemini_llm;
pub mod google_stt;
pub mod google_tts;
pub mod noop;
pub mod tinyhumans;
