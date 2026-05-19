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

/// Default Google STT model. `chirp_2` only works on the v2 endpoint;
/// we POST to v1 `speech:recognize`, so use a v1-valid model name.
const DEFAULT_MODEL: &str = "latest_long";
/// Default language code for Japanese.
const DEFAULT_LANGUAGE: &str = "ja-JP";
/// Environment variable checked when no API key is in config.
const ENV_API_KEY: &str = "GOOGLE_API_KEY";
/// Default Google Speech API base URL.
const DEFAULT_BASE_URL: &str = "https://speech.googleapis.com";

/// Google Cloud Speech-to-Text provider.
pub struct GoogleStt {
    api_key: String,
    model: String,
    language: String,
    base_url: String,
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

        let base_url = google_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        Ok(Self {
            api_key,
            model,
            language,
            base_url,
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

        // 3. POST to the v1 recognize endpoint. Pass the API key in the
        //    `X-goog-api-key` header rather than the URL query string —
        //    `reqwest`'s error Display includes the request URL, so a
        //    `?key=...` query param would leak the key into logs on any
        //    transport failure.
        let url = format!("{}/v1/speech:recognize", self.base_url);
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
            .header("X-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            // Intentionally drop the raw reqwest error — it formats the
            // request URL, which we want to keep out of logs even though
            // the key now lives in a header.
            .map_err(|_| "google STT HTTP request failed".to_string())?;

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
    use crate::openhuman::config::schema::ProviderConfig;
    use crate::openhuman::config::TEST_ENV_LOCK;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a `GoogleStt` pointed at a wiremock URI.
    fn stt_at(base_url: &str) -> GoogleStt {
        GoogleStt {
            api_key: "test-key".to_string(),
            model: "chirp_2".to_string(),
            language: "ja-JP".to_string(),
            base_url: base_url.to_string(),
            client: Client::new(),
        }
    }

    /// Verify that `from_config` fails gracefully when no key is available.
    #[test]
    fn from_config_rejects_missing_api_key() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Ensure env var is not set for this test.
        std::env::remove_var(ENV_API_KEY);
        let config = MeetAgentConfig::default();
        let result = GoogleStt::from_config(&config);
        assert!(result.is_err(), "should fail without API key");
    }

    #[tokio::test]
    async fn transcribe_returns_text_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/speech:recognize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "alternatives": [{ "transcript": "hello world" }]
                }]
            })))
            .mount(&server)
            .await;

        let stt = stt_at(&server.uri());
        let pcm = vec![0i16; 1600]; // 100ms silence @ 16kHz
        let text = stt.transcribe(&pcm, 16_000).await.expect("transcribe ok");
        assert_eq!(text, "hello world");
    }

    #[tokio::test]
    async fn transcribe_returns_error_on_api_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/speech:recognize"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let stt = stt_at(&server.uri());
        let result = stt.transcribe(&vec![0i16; 1600], 16_000).await;
        assert!(result.is_err(), "5xx response should produce Err");
        let err = result.unwrap_err();
        assert!(err.contains("500"), "error should mention status: {err}");
    }

    #[tokio::test]
    async fn transcribe_returns_empty_on_no_results() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/speech:recognize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": []
            })))
            .mount(&server)
            .await;

        let stt = stt_at(&server.uri());
        let text = stt
            .transcribe(&vec![0i16; 1600], 16_000)
            .await
            .expect("empty results should still parse");
        assert_eq!(text, "", "no results → empty transcript");
    }

    #[test]
    fn from_config_reads_env_var() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(ENV_API_KEY, "env-key-stt");
        let config = MeetAgentConfig::default();
        let result = GoogleStt::from_config(&config);
        // Clean up before assertion so failure doesn't leak the env var.
        std::env::remove_var(ENV_API_KEY);
        let stt = result.expect("env var should satisfy from_config");
        assert_eq!(stt.api_key, "env-key-stt");
    }

    #[test]
    fn from_config_reads_provider_config() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_API_KEY);
        let mut config = MeetAgentConfig::default();
        config.providers.insert(
            "google".to_string(),
            ProviderConfig {
                api_key: Some("config-key".to_string()),
                stt_model: Some("chirp_3".to_string()),
                ..Default::default()
            },
        );
        let stt = GoogleStt::from_config(&config).expect("config key should satisfy");
        assert_eq!(stt.api_key, "config-key");
        assert_eq!(stt.model, "chirp_3");
    }
}
