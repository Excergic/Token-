//! LlmClient: one request in, one response out. No tools, no loop, no streaming.
//!
//! Talks to the Sarvam AI chat completions API:
//! `POST https://api.sarvam.ai/v1/chat/completions`, auth via the
//! `api-subscription-key` header, OpenAI-shaped request and response bodies.

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://api.sarvam.ai";
pub const DEFAULT_MODEL: &str = "sarvam-105b";

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api returned {status}: {body}")]
    Api { status: u16, body: String },
    #[error("api returned no choices")]
    EmptyResponse,
}

pub struct LlmClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl LlmClient {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client with static config");
        Self {
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// Send a single user message and return the assistant's reply text.
    pub fn complete(&self, prompt: &str) -> Result<String, LlmError> {
        let request = ChatRequest {
            model: &self.model,
            messages: vec![Message {
                role: "user",
                content: prompt,
            }],
        };

        let response = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("api-subscription-key", &self.api_key)
            .json(&request)
            .send()?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: ChatResponse = response.json()?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .ok_or(LlmError::EmptyResponse)
    }
}

// Wire types. Kept private: nothing outside this module should know the
// provider's JSON shape.

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}
