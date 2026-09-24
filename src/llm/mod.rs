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
use std::io::{BufRead, BufReader};
use std::time::Duration;

use crate::conversation::{Message, ToolCall};
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
    #[error("the stream ended badly: {0}")]
    Stream(std::io::Error),
}

/// A tool call as it arrives in pieces. The chat wire sends a call's name and
/// its arguments across several chunks, keyed by position rather than by id,
/// so they are merged here before becoming a `ToolCall`.
#[derive(Default, Clone)]
pub struct PartialToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// What a transport has gathered so far. One turn's worth; each transport
/// fills the parts its wire actually sends.
#[derive(Default)]
pub struct StreamState {
    pub content: String,
    pub tool_calls: Vec<PartialToolCall>,
    pub finish_reason: String,
    /// Set by a wire that delivers the finished turn whole, rather than
    /// leaving it to be reassembled from deltas.
    pub completion: Option<Completion>,
}

impl StreamState {
    /// Merge a piece of a tool call arriving at `index`.
    pub fn merge_tool_call(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) {
        if self.tool_calls.len() <= index {
            self.tool_calls
                .resize(index + 1, PartialToolCall::default());
        }
        let call = &mut self.tool_calls[index];
        if let Some(id) = id {
            call.id.push_str(id);
        }
        if let Some(name) = name {
            call.name.push_str(name);
        }
        if let Some(arguments) = arguments {
            call.arguments.push_str(arguments);
        }
    }

    /// The assembled turn, for a wire that sends only deltas.
    pub fn into_completion(self) -> Completion {
        Completion {
            message: Message::Assistant {
                content: (!self.content.is_empty()).then_some(self.content),
                tool_calls: self
                    .tool_calls
                    .into_iter()
                    .filter(|call| !call.name.is_empty())
                    .map(|call| ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                    })
                    .collect(),
            },
            finish_reason: self.finish_reason,
        }
    }
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
trait Transport: Send {
    /// Path appended to the base URL.
    fn path(&self) -> &'static str;

    fn request(&self, model: &str, messages: &[Message], tools: &[ToolSpec]) -> Value;

    fn normalize(&self, body: &str) -> Result<Completion, LlmError>;

    /// Interpret one SSE payload. Returns text to show the user now, if this
    /// event carried any.
    fn on_event(&self, data: &str, state: &mut StreamState) -> Result<Option<String>, LlmError>;

    /// The finished turn, once the stream has ended.
    fn finish(&self, state: StreamState) -> Result<Completion, LlmError>;
}

fn transport_for(mode: ApiMode) -> Box<dyn Transport + Send> {
    match mode {
        ApiMode::ChatCompletions => Box::new(chat::ChatCompletions),
        ApiMode::OpenAiResponses => Box::new(responses::OpenAiResponses),
    }
}

/// Stateless. Holds no conversation: the runtime owns that and passes the
/// whole transcript every call, whichever provider is behind this.
/// How long a streamed answer may take in total. Generous on purpose: the
/// 60s that suits a single request is an answer's worth of time here.
const STREAM_TIMEOUT: Duration = Duration::from_secs(600);

pub struct LlmClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
    auth: Auth,
    transport: Box<dyn Transport + Send>,
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

    /// Like `send`, but hands each piece of text to `on_delta` as it arrives.
    ///
    /// The transcript still comes back as one `Completion`, so the runtime is
    /// unchanged: streaming is about when the user sees the answer, not about
    /// who owns the conversation.
    pub fn send_streaming(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<Completion, LlmError> {
        let mut body = self.transport.request(&self.model, messages, tools);
        body["stream"] = Value::Bool(true);
        let url = format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.transport.path()
        );

        let response = self
            .authorise(self.http.post(url))
            // The client's timeout covers a whole request, and a whole
            // request here is the length of an answer. A long one must not
            // be cut off at the deadline meant for a short one.
            .timeout(STREAM_TIMEOUT)
            .json(&body)
            .send()?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                body,
            });
        }

        let mut state = StreamState::default();
        for line in BufReader::new(response).lines() {
            let line = line.map_err(LlmError::Stream)?;
            // Server-sent events: `data:` carries the payload, everything
            // else is framing (`event:`, comments, blank separators).
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            // `[DONE]` closes a chat stream; the Responses wire has no
            // sentinel and ends with a terminal event instead.
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            if let Some(text) = self.transport.on_event(payload, &mut state)? {
                on_delta(&text);
            }
        }

        self.transport.finish(state)
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
