//! Provider-agnostic model access.
//!
//! The runtime hands over the conversation and the tool list and gets back a
//! `Completion` in this project's own types. Which HTTP API that actually
//! took is decided here by `ApiMode`, and each wire format lives in exactly
//! one transport module. Nothing above `llm` knows any provider's JSON.
//!
//! A transport is pure: it turns our types into a request body and a response
//! body back into our types. Everything shared (the HTTP client, the timeout,
//! auth, status handling) stays in `LlmClient`, so a transport can be tested
//! without a network.

mod chat;
mod responses;

use reqwest::blocking::{Client, RequestBuilder};
use serde_json::Value;
use std::time::Duration;

use crate::conversation::Message;
use crate::tools::ToolSpec;

/// What the model returned, plus why it stopped. `finish_reason` is the only
/// way to tell a finished answer from one cut short by a token limit.
pub struct Completion {
    pub message: Message,
    pub finish_reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api returned {status}: {body}")]
    Api { status: u16, body: String },
    #[error("could not decode api response: {source}; body: {body}")]
    Decode {
        source: serde_json::Error,
        body: String,
    },
    #[error("api returned no output")]
    EmptyResponse,
}

/// A provider, only as far as its defaults go. Which wire format it speaks is
/// `ApiMode`'s business, not this enum's: the two are deliberately separate,
/// because an OpenAI-compatible endpoint elsewhere still speaks chat
/// completions, and Sarvam serves two wire formats of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Provider {
    Openai,
    Sarvam,
}

impl Provider {
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Openai => "https://api.openai.com",
            Self::Sarvam => "https://api.sarvam.ai",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::Openai => "gpt-5.5",
            Self::Sarvam => "sarvam-105b",
        }
    }

    pub fn key_env(self) -> &'static str {
        match self {
            Self::Openai => "OPENAI_API_KEY",
            Self::Sarvam => "SARVAM_API_KEY",
        }
    }
}

/// Which HTTP API a request is sent to. `anthropic_messages` and
/// `bedrock_converse` slot in here without touching anything above `llm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ApiMode {
    /// OpenAI-shaped `POST /v1/chat/completions`. Sarvam speaks this.
    ChatCompletions,
    /// OpenAI `POST /v1/responses`, typed input and output items.
    #[value(name = "openai-responses")]
    OpenAiResponses,
}

/// Pick the wire format for an endpoint. Ordered ladder, first match wins:
/// an explicit override, then the host, then the fallback. Inference is done
/// on the host and never on the model name, so an unfamiliar model id cannot
/// silently change which API is called.
pub fn resolve_api_mode(explicit: Option<ApiMode>, base_url: &str) -> ApiMode {
    if let Some(mode) = explicit {
        return mode;
    }
    match host_of(base_url) {
        host if host.ends_with("openai.com") => ApiMode::OpenAiResponses,
        host if host.ends_with("sarvam.ai") => ApiMode::ChatCompletions,
        _ => ApiMode::ChatCompletions,
    }
}

/// How the key is presented. A property of the endpoint, not of the wire
/// format: Sarvam wants its own header on the same body shape OpenAI takes a
/// bearer token for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    Bearer,
    SarvamSubscriptionKey,
}

fn resolve_auth(base_url: &str) -> Auth {
    match host_of(base_url) {
        host if host.ends_with("sarvam.ai") => Auth::SarvamSubscriptionKey,
        _ => Auth::Bearer,
    }
}

/// Host of a base URL, without scheme, path or port. Deliberately not a URL
/// parser: a malformed value simply fails to match and falls to the default.
fn host_of(base_url: &str) -> &str {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let host = rest.split('/').next().unwrap_or(rest);
    host.split(':').next().unwrap_or(host)
}

/// One wire format. Both halves are pure so they can be tested on fixtures:
/// `request` renders our types, `normalize` reads the provider's reply back
/// into them. Whatever came in, the runtime always gets a `Completion`.
trait Transport {
    /// Path appended to the base URL.
    fn path(&self) -> &'static str;

    fn request(&self, model: &str, messages: &[Message], tools: &[ToolSpec]) -> Value;

    fn normalize(&self, body: &str) -> Result<Completion, LlmError>;
}

fn transport_for(mode: ApiMode) -> Box<dyn Transport> {
    match mode {
        ApiMode::ChatCompletions => Box::new(chat::ChatCompletions),
        ApiMode::OpenAiResponses => Box::new(responses::OpenAiResponses),
    }
}

/// Stateless. Holds no conversation: the runtime owns that and passes the
/// whole transcript every call, whichever provider is behind this.
pub struct LlmClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
    auth: Auth,
    transport: Box<dyn Transport>,
}

impl LlmClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        mode: ApiMode,
    ) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client with static config");
        let base_url = base_url.into();
        let auth = resolve_auth(&base_url);
        Self {
            http,
            base_url,
            api_key: api_key.into(),
            model: model.into(),
            auth,
            transport: transport_for(mode),
        }
    }

    /// Send the whole conversation plus the tools the model may call, and
    /// return the assistant's next message.
    pub fn send(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion, LlmError> {
        let body = self.transport.request(&self.model, messages, tools);
        let url = format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.transport.path()
        );

        let response = self.authorise(self.http.post(url)).json(&body).send()?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                body,
            });
        }

        self.transport.normalize(&response.text()?)
    }

    /// The model these requests name, for anything that records a run.
    pub fn model(&self) -> &str {
        &self.model
    }

    fn authorise(&self, request: RequestBuilder) -> RequestBuilder {
        match self.auth {
            Auth::Bearer => request.header("Authorization", format!("Bearer {}", self.api_key)),
            Auth::SarvamSubscriptionKey => request.header("api-subscription-key", &self.api_key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_mode_beats_the_host() {
        let mode = resolve_api_mode(Some(ApiMode::ChatCompletions), "https://api.openai.com");
        assert_eq!(mode, ApiMode::ChatCompletions);
    }

    #[test]
    fn hosts_pick_their_own_wire_format() {
        assert_eq!(
            resolve_api_mode(None, "https://api.openai.com"),
            ApiMode::OpenAiResponses
        );
        assert_eq!(
            resolve_api_mode(None, "https://api.sarvam.ai"),
            ApiMode::ChatCompletions
        );
    }

    #[test]
    fn an_unknown_host_falls_back_to_chat_completions() {
        // The common case for a self-hosted or proxied OpenAI-compatible endpoint.
        assert_eq!(
            resolve_api_mode(None, "http://localhost:11434/v1"),
            ApiMode::ChatCompletions
        );
    }

    #[test]
    fn auth_follows_the_host_not_the_wire_format() {
        assert_eq!(
            resolve_auth("https://api.sarvam.ai"),
            Auth::SarvamSubscriptionKey
        );
        assert_eq!(resolve_auth("https://api.openai.com"), Auth::Bearer);
    }

    #[test]
    fn host_parsing_survives_ports_paths_and_junk() {
        assert_eq!(host_of("https://api.openai.com/v1/"), "api.openai.com");
        assert_eq!(host_of("http://localhost:11434/v1"), "localhost");
        assert_eq!(host_of("api.sarvam.ai"), "api.sarvam.ai");
        assert_eq!(host_of(""), "");
    }
}
