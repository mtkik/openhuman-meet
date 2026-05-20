//! Google Meet integration settings.
//!
//! Currently exposes a single privacy-relevant flag:
//! `auto_orchestrator_handoff` — when `true`, ending a Google Meet call
//! inside the OpenHuman webview hands the captured transcript to the
//! orchestrator agent, which may **proactively** execute tools (e.g. post
//! summaries to Slack, draft messages, schedule follow-ups). Default
//! `false` so the user must opt in before any external action fires.
//!
//! Also contains `MeetAgentConfig` for provider selection (Phase 1 trait
//! extraction). The `meet_agent` sub-section allows selecting STT/LLM/TTS
//! providers independently.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MeetConfig {
    /// When `true`, the orchestrator agent receives the transcript of every
    /// completed Google Meet call as a fresh chat thread and is invited to
    /// take proactive actions on it (drafting messages, scheduling
    /// follow-ups, etc.). When `false` (the default), transcripts still
    /// land in memory but no auto-orchestrator handoff fires.
    #[serde(default = "default_auto_orchestrator_handoff")]
    pub auto_orchestrator_handoff: bool,

    /// Meet Agent provider configuration. When absent, all providers
    /// default to `tinyhumans` (the original backend).
    #[serde(default)]
    pub meet_agent: MeetAgentConfig,
}

fn default_auto_orchestrator_handoff() -> bool {
    false
}

/// Meet Agent provider configuration.
///
/// Allows independent selection of STT, LLM, and TTS providers.
/// Default is `tinyhumans` for all three (backward compatible).
///
/// ```toml
/// [meet.meet_agent]
/// stt_provider = "tinyhumans"
/// llm_provider = "tinyhumans"
/// tts_provider = "tinyhumans"
///
/// [meet.meet_agent.providers.google]
/// api_key = "..."
/// stt_model = "chirp_2"
/// tts_voice = "ja-JP-Standard-A"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MeetAgentConfig {
    #[serde(default = "default_tinyhumans")]
    pub stt_provider: String,

    #[serde(default = "default_tinyhumans")]
    pub llm_provider: String,

    #[serde(default = "default_tinyhumans")]
    pub tts_provider: String,

    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

fn default_tinyhumans() -> String {
    "tinyhumans".to_string()
}

/// Per-provider configuration. Fields are optional — providers read what
/// they need and ignore the rest.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub voice_id: Option<String>,
    pub stt_model: Option<String>,
    pub tts_voice: Option<String>,
    pub tts_model: Option<String>,
}

impl Default for MeetConfig {
    fn default() -> Self {
        Self {
            auto_orchestrator_handoff: false,
            meet_agent: MeetAgentConfig::default(),
        }
    }
}

impl Default for MeetAgentConfig {
    fn default() -> Self {
        Self {
            stt_provider: default_tinyhumans(),
            llm_provider: default_tinyhumans(),
            tts_provider: default_tinyhumans(),
            providers: HashMap::new(),
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            model: None,
            base_url: None,
            voice_id: None,
            stt_model: None,
            tts_voice: None,
            tts_model: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_disables_handoff() {
        let cfg = MeetConfig::default();
        assert!(
            !cfg.auto_orchestrator_handoff,
            "auto_orchestrator_handoff must default to false (privacy-conservative)"
        );
    }

    #[test]
    fn default_helper_returns_false() {
        assert!(!default_auto_orchestrator_handoff());
    }

    #[test]
    fn deserialize_missing_optional_fields_uses_defaults() {
        let cfg: MeetConfig = serde_json::from_value(json!({})).unwrap();
        assert!(
            !cfg.auto_orchestrator_handoff,
            "missing field must deserialize to false"
        );
    }

    #[test]
    fn deserialize_respects_explicit_handoff_flag() {
        let cfg: MeetConfig = serde_json::from_value(json!({
            "auto_orchestrator_handoff": true
        }))
        .unwrap();
        assert!(cfg.auto_orchestrator_handoff);
    }

    #[test]
    fn round_trip_preserves_handoff_flag() {
        let original = MeetConfig {
            auto_orchestrator_handoff: true,
            meet_agent: MeetAgentConfig::default(),
        };
        let s = serde_json::to_string(&original).unwrap();
        let back: MeetConfig = serde_json::from_str(&s).unwrap();
        assert!(back.auto_orchestrator_handoff);
    }

    #[test]
    fn meet_agent_config_defaults_to_tinyhumans() {
        let cfg = MeetAgentConfig::default();
        assert_eq!(cfg.stt_provider, "tinyhumans");
        assert_eq!(cfg.llm_provider, "tinyhumans");
        assert_eq!(cfg.tts_provider, "tinyhumans");
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn meet_agent_config_parses_provider_selection() {
        let cfg: MeetAgentConfig = serde_json::from_value(json!({
            "stt_provider": "google",
            "llm_provider": "gemini",
            "tts_provider": "noop"
        }))
        .unwrap();
        assert_eq!(cfg.stt_provider, "google");
        assert_eq!(cfg.llm_provider, "gemini");
        assert_eq!(cfg.tts_provider, "noop");
    }

    #[test]
    fn meet_agent_config_parses_provider_configs() {
        let cfg: MeetAgentConfig = serde_json::from_value(json!({
            "stt_provider": "google",
            "providers": {
                "google": {
                    "api_key": "test-key",
                    "stt_model": "chirp_2"
                }
            }
        }))
        .unwrap();
        let google = cfg.providers.get("google").expect("google provider");
        assert_eq!(google.api_key.as_deref(), Some("test-key"));
        assert_eq!(google.stt_model.as_deref(), Some("chirp_2"));
    }

    #[test]
    fn meet_config_toml_round_trip_with_meet_agent() {
        let original = MeetConfig {
            auto_orchestrator_handoff: false,
            meet_agent: MeetAgentConfig {
                stt_provider: "google".to_string(),
                llm_provider: "gemini".to_string(),
                tts_provider: "tinyhumans".to_string(),
                providers: {
                    let mut m = HashMap::new();
                    m.insert(
                        "google".to_string(),
                        ProviderConfig {
                            api_key: Some("key".to_string()),
                            ..Default::default()
                        },
                    );
                    m
                },
            },
        };
        let toml_str = toml::to_string(&original).unwrap();
        let back: MeetConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(back.meet_agent.stt_provider, "google");
        assert_eq!(back.meet_agent.llm_provider, "gemini");
        assert!(back.meet_agent.providers.contains_key("google"));
    }
}
