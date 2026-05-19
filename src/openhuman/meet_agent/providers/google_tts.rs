//! Google Cloud Text-to-Speech provider.
//!
//! Uses the v1 REST API (`texttospeech.googleapis.com/v1/text:synthesize`) with
//! an API-key query parameter. Returns LINEAR16 PCM at the requested sample rate,
//! which we decode from base64 and convert to `Vec<i16>`.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};

use super::TextToSpeech;
use crate::openhuman::config::schema::MeetAgentConfig;

/// Default TTS voice name (Japanese Standard-A).
const DEFAULT_VOICE: &str = "ja-JP-Standard-A";
/// Default language code.
const DEFAULT_LANGUAGE: &str = "ja-JP";
/// Environment variable checked when no API key is in config.
const ENV_API_KEY: &str = "GOOGLE_API_KEY";

/// Google Cloud Text-to-Speech provider.
pub struct GoogleTts {
    api_key: String,
    voice: String,
    language: String,
    client: Client,
}

impl GoogleTts {
    /// Build a `GoogleTts` from the `meet_agent` TOML config section.
    ///
    /// Reads the API key from `providers.google.api_key`, falling back to
    /// the `GOOGLE_API_KEY` environment variable. Google STT and TTS share
    /// the same API key via the `google` provider config section.
    pub fn from_config(config: &MeetAgentConfig) -> Result<Self, String> {
        let google_cfg = config.providers.get("google");
        let api_key = google_cfg
            .and_then(|c| c.api_key.clone())
            .or_else(|| std::env::var(ENV_API_KEY).ok())
            .ok_or_else(|| {
                format!(
                    "google TTS: no API key in config or ${ENV_API_KEY} env var"
                )
            })?;

        let voice = google_cfg
            .and_then(|c| c.tts_voice.clone())
            .unwrap_or_else(|| DEFAULT_VOICE.to_string());

        let language = DEFAULT_LANGUAGE.to_string();

        Ok(Self {
            api_key,
            voice,
            language,
            client: Client::new(),
        })
    }
}

#[async_trait]
impl TextToSpeech for GoogleTts {
    async fn synthesize(&self, text: &str, sample_rate: u32) -> Result<Vec<i16>, String> {
        // 1. POST to Google TTS endpoint.
        let url = format!(
            "https://texttospeech.googleapis.com/v1/text:synthesize?key={}",
            self.api_key
        );
        let body = json!({
            "input": { "text": text },
            "voice": {
                "languageCode": self.language,
                "name": self.voice,
            },
            "audioConfig": {
                "audioEncoding": "LINEAR16",
                "sampleRateHertz": sample_rate,
            }
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("google TTS request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("google TTS HTTP {status}: {text}"));
        }

        let raw: Value = resp
            .json()
            .await
            .map_err(|e| format!("google TTS JSON parse: {e}"))?;

        // 2. Parse: audioContent (base64 encoded audio).
        let audio_b64 = raw
            .get("audioContent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("google TTS missing audioContent: {raw}"))?;

        // 3. Decode base64 → bytes.
        let pcm_bytes = B64
            .decode(audio_b64.as_bytes())
            .map_err(|e| format!("google TTS base64 decode: {e}"))?;

        if !pcm_bytes.len().is_multiple_of(2) {
            return Err(format!(
                "google TTS odd byte length: {}",
                pcm_bytes.len()
            ));
        }

        // 4. Convert bytes to Vec<i16> (PCM16LE).
        let samples: Vec<i16> = pcm_bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();

        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `from_config` fails gracefully when no key is available.
    #[test]
    fn from_config_rejects_missing_api_key() {
        std::env::remove_var(ENV_API_KEY);
        let config = MeetAgentConfig::default();
        let result = GoogleTts::from_config(&config);
        assert!(result.is_err(), "should fail without API key");
    }
}
