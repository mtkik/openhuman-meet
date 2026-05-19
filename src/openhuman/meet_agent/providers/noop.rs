//! No-op provider implementations for testing.
//!
//! STT returns empty string, LLM returns empty string, TTS returns empty vec.
//! Useful for unit tests that need to exercise the brain turn pipeline
//! without any real backend calls.

use async_trait::async_trait;

use super::{ConversationTurn, MeetingLLM, SpeechToText, TextToSpeech};

/// No-op STT provider. Always returns an empty string.
pub struct NoopStt;

#[async_trait]
impl SpeechToText for NoopStt {
    async fn transcribe(&self, _pcm: &[i16], _sample_rate: u32) -> Result<String, String> {
        Ok(String::new())
    }
}

/// No-op LLM provider. Always returns an empty string.
pub struct NoopLlm;

#[async_trait]
impl MeetingLLM for NoopLlm {
    async fn reply(
        &self,
        _prompt: &str,
        _history: &[ConversationTurn],
        _system_prompt: &str,
        _max_tokens: u32,
    ) -> Result<String, String> {
        Ok(String::new())
    }
}

/// No-op TTS provider. Always returns an empty vec.
pub struct NoopTts;

#[async_trait]
impl TextToSpeech for NoopTts {
    async fn synthesize(&self, _text: &str, _sample_rate: u32) -> Result<Vec<i16>, String> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_stt_returns_empty() {
        let stt = NoopStt;
        let result = stt.transcribe(&[0i16; 1600], 16000).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn noop_llm_returns_empty() {
        let llm = NoopLlm;
        let result = llm
            .reply("hello", &[], "system", 100)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn noop_tts_returns_empty() {
        let tts = NoopTts;
        let result = tts.synthesize("hello", 16000).await.unwrap();
        assert!(result.is_empty());
    }
}
