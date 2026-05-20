# Phase 1 Implementation Spec: Provider Trait Extraction

## Context
Based on DESIGN.md decisions:
- **Trait design**: 3 independent traits (STT / LLM / TTS)
- **Default providers**: Google STT + Gemini Flash + Google TTS
- **Headless support**: From Phase 1
- **Config format**: TOML

## Task 1: Create Provider Traits

### File: `src/openhuman/meet_agent/providers/mod.rs` (NEW)

```rust
//! Provider traits for the Meet Agent pipeline.
//! 
//! Three independent traits allow mixing providers:
//! - SpeechToText: PCM → text
//! - MeetingLLM: prompt + history → reply text
//! - TextToSpeech: text → PCM

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Conversation turn for LLM history context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub role: String,
    pub content: String,
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

/// Provider factory — creates providers from config.
pub mod factory;
pub mod google_stt;
pub mod gemini_llm;
pub mod google_tts;
pub mod tinyhumans; // existing backend (default for backward compat)
pub mod noop;       // no-op provider for testing
```

### File: `src/openhuman/meet_agent/providers/factory.rs` (NEW)

Reads config from `[meet_agent]` section and instantiates the correct providers.

### File: `src/openhuman/meet_agent/providers/google_stt.rs` (NEW)

Google Cloud Speech-to-Text v2 implementation:
- Uses `GOOGLE_API_KEY` env var or `[meet_agent.providers.google].api_key`
- PCM → base64 WAV → Google REST API → text
- Endpoint: `https://speech.googleapis.com/v2/projects/.../locations/global/recognizers/...:recognize`

### File: `src/openhuman/meet_agent/providers/gemini_llm.rs` (NEW)

Google Gemini implementation:
- Uses `GEMINI_API_KEY` env var or `[meet_agent.providers.gemini].api_key`
- OpenAI-compatible chat completions via `https://generativelanguage.googleapis.com/v1beta/openai/`
- Model default: `gemini-2.5-flash`

### File: `src/openhuman/meet_agent/providers/google_tts.rs` (NEW)

Google Cloud Text-to-Speech implementation:
- Uses `GOOGLE_API_KEY` env var or same Google config
- Endpoint: `https://texttospeech.googleapis.com/v1/text:synthesize`
- Returns base64 audio → decode to PCM

### File: `src/openhuman/meet_agent/providers/tinyhumans.rs` (NEW)

Wraps the existing `cloud_transcribe`, `BackendOAuthClient`, `reply_speech` calls.
This is the default for backward compatibility.

### File: `src/openhuman/meet_agent/providers/noop.rs` (NEW)

No-op provider for testing. STT returns "", LLM returns "", TTS returns empty vec.

## Task 2: Modify brain.rs

### Changes to `src/openhuman/meet_agent/brain.rs`

Replace the 3 hardcoded functions with trait calls:

```rust
// Before:
async fn stt(samples: &[i16]) -> Result<String, String> { ... }
async fn llm_meeting(prompt: &str, history: &[ConversationTurn]) -> Result<String, String> { ... }
async fn tts(text: &str) -> Result<Vec<i16>, String> { ... }

// After:
// Providers are resolved once per session via the factory, stored in the session struct
// brain.rs calls self.stt_provider.transcribe(), self.llm_provider.reply(), self.tts_provider.synthesize()
```

The key change:
1. `run_turn()` and `run_caption_turn()` take provider references as parameters
2. Provider instances are created when a session starts and stored alongside the session
3. The existing tinyhumans path remains as the `tinyhumans` provider implementation

## Task 3: Add Config Schema

### File: `src/openhuman/config/schema/meet.rs` (MODIFY or NEW)

Add TOML config section:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct MeetAgentConfig {
    #[serde(default = "default_stt_provider")]
    pub stt_provider: String,
    #[serde(default = "default_llm_provider")]  
    pub llm_provider: String,
    #[serde(default = "default_tts_provider")]
    pub tts_provider: String,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    // provider-specific fields
    pub voice_id: Option<String>,
    pub stt_model: Option<String>,
    pub tts_voice: Option<String>,
    pub tts_model: Option<String>,
}

fn default_stt_provider() -> String { "tinyhumans".into() }
fn default_llm_provider() -> String { "tinyhumans".into() }
fn default_tts_provider() -> String { "tinyhumans".into() }
```

## Task 4: Wire into session.rs

Store provider instances in `MeetAgentSession` so they live for the session lifetime.

## Task 5: Tests

1. Unit tests for each provider (mock HTTP)
2. Integration test: brain.rs with noop providers
3. Config parsing test
4. Existing tests must still pass (backward compat)

## Critical Constraints

1. **Zero breaking changes**: Default config = existing tinyhumans behavior
2. **Existing tests must pass**: Don't break meet_agent/ops_tests.rs, brain.rs tests
3. **async_trait crate**: Already in dependencies (check Cargo.toml)
4. **No new system deps**: HTTP-only providers (reqwest already available)
5. **Rust edition 2021, toolchain 1.93.0**: Check compatibility

## Files NOT to modify
- meet_scanner/ (CDP automation)
- meet_call/ (window management)  
- meet_audio/ (audio pipeline)
- meet_video/ (camera frames)
- cdp/ (CDP connection)
- recipes/ (browser JS)
- meet_agent/session.rs (VAD/ring buffer) — only minor changes to add provider storage
- meet_agent/ops.rs (pure logic)
- meet_agent/types.rs (types)
- meet_agent/wav.rs (PCM utils)
- meet_agent/rpc.rs (JSON-RPC handlers) — only if needed for provider injection
