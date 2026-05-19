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
/// Default Google Text-to-Speech API base URL.
const DEFAULT_BASE_URL: &str = "https://texttospeech.googleapis.com";

/// Google Cloud Text-to-Speech provider.
pub struct GoogleTts {
    api_key: String,
    voice: String,
    language: String,
    base_url: String,
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

        let base_url = google_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        Ok(Self {
            api_key,
            voice,
            language,
            base_url,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("http client build"),
        })
    }
}

#[async_trait]
impl TextToSpeech for GoogleTts {
    async fn synthesize(&self, text: &str, sample_rate: u32) -> Result<Vec<i16>, String> {
        // 1. POST to Google TTS endpoint. Pass the API key via the
        //    `X-goog-api-key` header — `reqwest`'s error Display includes
        //    the URL, so a `?key=...` query param would leak the key
        //    into logs on transport failures.
        let url = format!("{}/v1/text:synthesize", self.base_url);
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
            .header("X-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            // Drop the raw reqwest error — it formats the request URL,
            // which we keep out of logs even though the key now lives
            // in a header.
            .map_err(|_| "google TTS HTTP request failed".to_string())?;

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

        // 4. Google's LINEAR16 response is a complete WAV file (44-byte
        //    RIFF header + PCM data), but the shell expects headerless
        //    PCM16. Strip the header when present so the first ~22ms of
        //    output isn't noise.
        let pcm_data: &[u8] = if pcm_bytes.starts_with(b"RIFF") {
            const WAV_HEADER_LEN: usize = 44;
            if pcm_bytes.len() > WAV_HEADER_LEN {
                &pcm_bytes[WAV_HEADER_LEN..]
            } else {
                &pcm_bytes[..]
            }
        } else {
            &pcm_bytes[..]
        };

        if !pcm_data.len().is_multiple_of(2) {
            return Err(format!(
                "google TTS odd byte length: {}",
                pcm_data.len()
            ));
        }

        // 5. Convert bytes to Vec<i16> (PCM16LE).
        let samples: Vec<i16> = pcm_data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();

        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openhuman::config::TEST_ENV_LOCK;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a `GoogleTts` pointed at a wiremock URI.
    fn tts_at(base_url: &str) -> GoogleTts {
        GoogleTts {
            api_key: "test-key".to_string(),
            voice: "ja-JP-Standard-A".to_string(),
            language: "ja-JP".to_string(),
            base_url: base_url.to_string(),
            client: Client::new(),
        }
    }

    /// Encode 100ms of silence (PCM16LE @ 16kHz, 1600 samples = 3200 bytes) as base64.
    fn silence_b64() -> String {
        let bytes = vec![0u8; 3200];
        B64.encode(&bytes)
    }

    /// Verify that `from_config` fails gracefully when no key is available.
    #[test]
    fn from_config_rejects_missing_api_key() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_API_KEY);
        let config = MeetAgentConfig::default();
        let result = GoogleTts::from_config(&config);
        assert!(result.is_err(), "should fail without API key");
    }

    #[tokio::test]
    async fn synthesize_returns_pcm() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text:synthesize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "audioContent": silence_b64(),
            })))
            .mount(&server)
            .await;

        let tts = tts_at(&server.uri());
        let pcm = tts.synthesize("hello", 16_000).await.expect("synth ok");
        // 3200 bytes → 1600 i16 samples.
        assert_eq!(pcm.len(), 1600);
        assert!(pcm.iter().all(|&s| s == 0));
    }

    #[tokio::test]
    async fn synthesize_handles_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text:synthesize"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let tts = tts_at(&server.uri());
        let result = tts.synthesize("hello", 16_000).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("500"));
    }

    #[tokio::test]
    async fn synthesize_handles_invalid_base64() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text:synthesize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "audioContent": "not-valid-base64!!!",
            })))
            .mount(&server)
            .await;

        let tts = tts_at(&server.uri());
        let result = tts.synthesize("hello", 16_000).await;
        assert!(result.is_err(), "invalid base64 should error");
        let err = result.unwrap_err();
        assert!(
            err.contains("base64"),
            "error should mention base64: {err}"
        );
    }

    #[tokio::test]
    async fn synthesize_handles_missing_audio_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text:synthesize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let tts = tts_at(&server.uri());
        let result = tts.synthesize("hello", 16_000).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("audioContent"));
    }
}
