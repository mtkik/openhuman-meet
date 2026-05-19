//! Gemini LLM provider via Google's OpenAI-compatible endpoint.
//!
//! Uses `https://generativelanguage.googleapis.com/v1beta/openai/chat/completions`
//! with `Authorization: Bearer <API_KEY>`. This avoids the need for the
//! Google AI SDK — a plain OpenAI-compatible HTTP call suffices.

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use super::MeetingLLM;
use super::super::providers::tinyhumans::strip_for_speech;
use crate::openhuman::config::schema::MeetAgentConfig;

/// Default Gemini model.
const DEFAULT_MODEL: &str = "gemini-2.5-flash";
/// Default OpenAI-compatible base URL for Gemini.
const DEFAULT_BASE_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/openai";
/// Environment variable checked when no API key is in config.
const ENV_API_KEY: &str = "GEMINI_API_KEY";

/// Gemini LLM provider using the OpenAI-compatible chat completions API.
pub struct GeminiLlm {
    api_key: String,
    model: String,
    base_url: String,
    client: Client,
}

impl GeminiLlm {
    /// Build a `GeminiLlm` from the `meet_agent` TOML config section.
    ///
    /// Reads the API key from `providers.gemini.api_key`, falling back to
    /// the `GEMINI_API_KEY` environment variable.
    pub fn from_config(config: &MeetAgentConfig) -> Result<Self, String> {
        let gemini_cfg = config.providers.get("gemini");
        let api_key = gemini_cfg
            .and_then(|c| c.api_key.clone())
            .or_else(|| std::env::var(ENV_API_KEY).ok())
            .ok_or_else(|| {
                format!(
                    "gemini LLM: no API key in config or ${ENV_API_KEY} env var"
                )
            })?;

        let model = gemini_cfg
            .and_then(|c| c.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let base_url = gemini_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        Ok(Self {
            api_key,
            model,
            base_url,
            client: Client::new(),
        })
    }
}

#[async_trait]
impl MeetingLLM for GeminiLlm {
    async fn reply(
        &self,
        prompt: &str,
        history: &[super::ConversationTurn],
        system_prompt: &str,
        max_tokens: u32,
    ) -> Result<String, String> {
        // 1. Build messages array from system_prompt + history + prompt.
        let mut messages: Vec<Value> = Vec::with_capacity(history.len() + 2);
        messages.push(json!({ "role": "system", "content": system_prompt }));
        for turn in history {
            messages.push(json!({ "role": turn.role, "content": turn.content }));
        }
        messages.push(json!({ "role": "user", "content": prompt }));

        // 2. POST to {base_url}/chat/completions.
        let url = format!("{}/chat/completions", self.base_url);
        let body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": max_tokens,
            "temperature": 0.5,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("gemini LLM request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("gemini LLM HTTP {status}: {text}"));
        }

        let raw: Value = resp
            .json()
            .await
            .map_err(|e| format!("gemini LLM JSON parse: {e}"))?;

        // 3. Parse: choices[0].message.content
        let content = raw
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| format!("gemini LLM unexpected response: {raw}"))?;

        // 4. Apply strip_for_speech for TTS-friendly output.
        Ok(strip_for_speech(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::ConversationTurn;
    use crate::openhuman::config::TEST_ENV_LOCK;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a `GeminiLlm` pointed at a wiremock URI.
    fn llm_at(base_url: &str) -> GeminiLlm {
        GeminiLlm {
            api_key: "test-key".to_string(),
            model: "gemini-2.5-flash".to_string(),
            base_url: base_url.to_string(),
            client: Client::new(),
        }
    }

    fn chat_response(content: &str) -> Value {
        json!({
            "id": "test",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }]
        })
    }

    /// Verify that `from_config` fails gracefully when no key is available.
    #[test]
    fn from_config_rejects_missing_api_key() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_API_KEY);
        let config = MeetAgentConfig::default();
        let result = GeminiLlm::from_config(&config);
        assert!(result.is_err(), "should fail without API key");
    }

    #[tokio::test]
    async fn reply_returns_text_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response("Hi there")))
            .mount(&server)
            .await;

        let llm = llm_at(&server.uri());
        let reply = llm
            .reply("hello", &[], "system", 100)
            .await
            .expect("reply ok");
        assert!(reply.contains("Hi there"), "got: {reply}");
    }

    #[tokio::test]
    async fn reply_strips_markdown_for_speech() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response(
                "**bold** and `code` and more\n```\nfenced\n```\n- bullet",
            )))
            .mount(&server)
            .await;

        let llm = llm_at(&server.uri());
        let reply = llm
            .reply("hi", &[], "sys", 100)
            .await
            .expect("reply ok");
        assert!(!reply.contains('*'), "asterisks should be stripped: {reply}");
        assert!(!reply.contains('`'), "backticks should be stripped: {reply}");
        assert!(!reply.contains("fenced"), "fenced block should be removed: {reply}");
    }

    #[tokio::test]
    async fn reply_handles_empty_choices() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "choices": [] })))
            .mount(&server)
            .await;

        let llm = llm_at(&server.uri());
        let result = llm.reply("hi", &[], "sys", 100).await;
        assert!(result.is_err(), "empty choices should error: {result:?}");
    }

    #[tokio::test]
    async fn reply_handles_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let llm = llm_at(&server.uri());
        let result = llm.reply("hi", &[], "sys", 100).await;
        assert!(result.is_err(), "5xx should error");
        assert!(result.unwrap_err().contains("500"));
    }

    #[tokio::test]
    async fn reply_includes_history() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response("ok")))
            .mount(&server)
            .await;

        let llm = llm_at(&server.uri());
        let history = vec![
            ConversationTurn {
                role: "user".to_string(),
                content: "previous question".to_string(),
            },
            ConversationTurn {
                role: "assistant".to_string(),
                content: "previous answer".to_string(),
            },
        ];
        llm.reply("now what", &history, "sys-prompt", 100)
            .await
            .expect("reply ok");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let messages = body["messages"].as_array().expect("messages array");
        // system + 2 history + user prompt = 4
        assert_eq!(messages.len(), 4, "messages: {messages:?}");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "sys-prompt");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "previous question");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "previous answer");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "now what");
    }

    #[tokio::test]
    async fn reply_sends_bearer_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response("ok")))
            .mount(&server)
            .await;

        let llm = llm_at(&server.uri());
        llm.reply("hi", &[], "sys", 100).await.expect("reply ok");

        let requests = server.received_requests().await.unwrap();
        let auth = requests[0]
            .headers
            .get("authorization")
            .expect("authorization header present");
        assert_eq!(auth.to_str().unwrap(), "Bearer test-key");
    }
}
