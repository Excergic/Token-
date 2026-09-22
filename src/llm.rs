//! LlmClient: stateless. Takes the conversation so far, returns the model's
//! next message. No tools are advertised, no loop, no streaming.
//!
//! Talks to the Sarvam AI chat completions API:
//! `POST https://api.sarvam.ai/v1/chat/completions`, auth via the
//! `api-subscription-key` header, OpenAI-shaped request and response bodies.

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::conversation::{Message, ToolCall};

/// What the model returned, plus why it stopped. `finish_reason` is the only
/// way to tell a finished answer from one truncated by `max_tokens`.
pub struct Completion {
    pub message: Message,
    pub finish_reason: String,
}

pub const DEFAULT_BASE_URL: &str = "https://api.sarvam.ai";
pub const DEFAULT_MODEL: &str = "sarvam-105b";

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

    /// Send the whole conversation plus the tools the model may call, and
    /// return the assistant's next message.
    pub fn chat(&self, messages: &[Message], tools: &[Value]) -> Result<Completion, LlmError> {
        let request = ChatRequest {
            model: &self.model,
            messages: messages.iter().map(WireMessage::from).collect(),
            tools: (!tools.is_empty()).then_some(tools),
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

        let body = response.text()?;
        let parsed: ChatResponse =
            serde_json::from_str(&body).map_err(|source| LlmError::Decode { source, body })?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| Completion {
                message: choice.message.into(),
                finish_reason: choice.finish_reason,
            })
            .ok_or(LlmError::EmptyResponse)
    }
}

// Wire types. Kept private: nothing outside this module should know the
// provider's JSON shape.

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [Value]>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    r#type: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

impl<'a> From<&'a Message> for WireMessage<'a> {
    fn from(message: &'a Message) -> Self {
        let empty = Self {
            role: "",
            content: None,
            tool_calls: None,
            tool_call_id: None,
        };
        match message {
            Message::System(content) => Self {
                role: "system",
                content: Some(content),
                ..empty
            },
            Message::User(content) => Self {
                role: "user",
                content: Some(content),
                ..empty
            },
            Message::Assistant {
                content,
                tool_calls,
            } => Self {
                role: "assistant",
                content: content.as_deref(),
                tool_calls: (!tool_calls.is_empty()).then(|| {
                    tool_calls
                        .iter()
                        .map(|call| WireToolCall {
                            id: &call.id,
                            r#type: "function",
                            function: WireFunction {
                                name: &call.name,
                                arguments: &call.arguments,
                            },
                        })
                        .collect()
                }),
                ..empty
            },
            Message::Tool {
                tool_call_id,
                content,
            } => Self {
                role: "tool",
                content: Some(content),
                tool_call_id: Some(tool_call_id),
                ..empty
            },
        }
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: String,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    /// The API sends `null` (not a missing field) when there are no calls.
    tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Deserialize)]
struct ResponseToolCall {
    id: String,
    function: ResponseFunction,
}

#[derive(Deserialize)]
struct ResponseFunction {
    name: String,
    arguments: String,
}

impl From<ResponseMessage> for Message {
    fn from(message: ResponseMessage) -> Self {
        Message::Assistant {
            content: message.content,
            tool_calls: message
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .map(|call| ToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serialises_every_message_variant() {
        let messages = [
            Message::System("be brief".into()),
            Message::User("hi".into()),
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"src/main.rs"}"#.into(),
                }],
            },
            Message::Tool {
                tool_call_id: "call-1".into(),
                content: "fn main() {}".into(),
            },
            Message::Assistant {
                content: Some("done".into()),
                tool_calls: vec![],
            },
        ];
        let wire: Vec<WireMessage> = messages.iter().map(WireMessage::from).collect();
        let actual = serde_json::to_value(&wire).unwrap();

        let expected = json!([
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}"}
            }]},
            {"role": "tool", "content": "fn main() {}", "tool_call_id": "call-1"},
            {"role": "assistant", "content": "done"},
        ]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn parses_text_response() {
        let body = json!({
            "choices": [{
                "finish_reason": "stop",
                "index": 0,
                "message": {"role": "assistant", "content": "hello", "tool_calls": null}
            }]
        });
        let parsed: ChatResponse = serde_json::from_value(body).unwrap();
        let message: Message = parsed.choices.into_iter().next().unwrap().message.into();
        assert_eq!(
            message,
            Message::Assistant {
                content: Some("hello".into()),
                tool_calls: vec![],
            }
        );
    }

    #[test]
    fn parses_tool_call_response() {
        let body = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-abc",
                        "type": "function",
                        "function": {"name": "cargo_test", "arguments": "{}"}
                    }]
                }
            }]
        });
        let parsed: ChatResponse = serde_json::from_value(body).unwrap();
        let message: Message = parsed.choices.into_iter().next().unwrap().message.into();
        assert_eq!(
            message,
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-abc".into(),
                    name: "cargo_test".into(),
                    arguments: "{}".into(),
                }],
            }
        );
    }
}
