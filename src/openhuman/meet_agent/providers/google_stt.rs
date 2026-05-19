//! Google Cloud Speech-to-Text provider.
//!
//! Uses the v1 REST API (`speech.googleapis.com/v1/speech:recognize`) with
//! an API-key query parameter. This is the simplest approach — no service
//! account credentials or ADC required.
//!
//! Audio is sent as a base64-encoded WAV (LINEAR16, mono).

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};

use super::SpeechToText;
use crate::openhuman::config::schema::MeetAgentConfig;
use crate::openhuman::meet_agent::wav;

/// Default Google STT model (Chirp 2).
const DEFAULT_MODEL: &str = "chirp_2";
/// Default language code for Japanese.
const DEFAULT_LANGUAGE: &str = "ja-JP";
/// Environment variable checked when no API key is in config.
const ENV_API_KEY: &str = "GOOGLE_API_KEY";

/// Google Cloud Speech-to-Text provider.
pub struct GoogleStt {
    api_key: String,
    model: String,
    language: String,
    client: Client,
}

impl GoogleStt {
    /// Build a `GoogleStt` from the `meet_agent` TOML config section.
    ///
    /// Reads the API key from `providers.google.api_key`, falling back to
    /// the `GOOGLE_API_KEY` environment variable.
    pub fn from_config(config: &MeetAgentConfig) -> Result<Self, String> {
        let google_cfg = config.providers.get("google");
        let api_key = google_cfg
            .and_then(|c| c.api_key.clone())
            .or_else(|| std::env::var(ENV_API_KEY).ok())
            .ok_or_else(|| {
                format!(
                    "google STT: no API key in config or ${ENV_API_KEY} env var"
                )
            })?;

        let model = google_cfg
            .and_then(|c| c.stt_model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let language = DEFAULT_LANGUAGE.to_string();

        Ok(Self {
            api_key,
            model,
            language,
            client: Client::new(),
        })
    }
}

#[async_trait]
impl SpeechToText for GoogleStt {
    async fn transcribe(&self, pcm: &[i16], sample_rate: u32) -> Result<String, String> {
        // 1. Pack PCM into WAV.
        let wav_bytes = wav::pack_pcm16le_mono_wav(pcm, sample_rate);

        // 2. Encode WAV bytes as base64.
        let audio_b64 = B64.encode(&wav_bytes);

        // 3. POST to the v1 recognize endpoint.
        let url = format!(
            "https://speech.googleapis.com/v1/speech:recognize?key={}",
            self.api_key
        );
        let body = json!({
            "config": {
                "encoding": "LINEAR16",
                "sampleRateHertz": sample_rate,
                "languageCode": self.language,
                "model": self.model,
            },
            "audio": {
                "content": audio_b64,
            }
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("google STT request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("google STT HTTP {status}: {text}"));
        }

        let raw: Value = resp
            .json()
            .await
            .map_err(|e| format!("google STT JSON parse: {e}"))?;

        // 4. Parse: results[0].alternatives[0].transcript
        let transcript = raw
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("alternatives"))
            .and_then(|a| a.as_array())
            .and_then(|arr| arr.first())
            .and_then(|alt| alt.get("transcript"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        Ok(transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `from_config` fails gracefully when no key is available.
    #[test]
    fn from_config_rejects_missing_api_key() {
        // Ensure env var is not set for this test.
        std::env::remove_var(ENV_API_KEY);
        let config = MeetAgentConfig::default();
        let result = GoogleStt::from_config(&config);
        assert!(result.is_err(), "should fail without API key");
    }
}
