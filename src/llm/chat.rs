//! The OpenAI-shaped chat completions wire: `POST /v1/chat/completions`.
//!
//! Sarvam speaks this, as does any OpenAI-compatible endpoint. One flat
//! assistant message carries content and tool calls as sibling fields, so the
//! order between them is not expressible; that is the format's limit, not ours.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Completion, LlmError, Transport};
use crate::conversation::{Message, ToolCall};
use crate::tools::ToolSpec;

pub(super) struct ChatCompletions;

impl Transport for ChatCompletions {
    fn path(&self) -> &'static str {
        "/v1/chat/completions"
    }

    fn request(&self, model: &str, messages: &[Message], tools: &[ToolSpec]) -> Value {
        let wire: Vec<WireMessage> = messages.iter().map(WireMessage::from).collect();
        let mut body = json!({
            "model": model,
            "messages": wire,
        });
        if !tools.is_empty() {
            body["tools"] = tools.iter().map(tool_json).collect();
        }
        body
    }

    fn normalize(&self, body: &str) -> Result<Completion, LlmError> {
        let parsed: ChatResponse =
            serde_json::from_str(body).map_err(|source| LlmError::Decode {
                source,
                body: body.to_string(),
            })?;
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

/// This wire nests the function under a `function` key. The Responses wire
/// does not; that difference is the whole reason `ToolSpec` is neutral.
fn tool_json(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.parameters,
        }
    })
}

// Wire types. Kept private: nothing outside this module should know the shape.

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

    fn spec() -> ToolSpec {
        ToolSpec {
            name: "read_file",
            description: "Read a file.",
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

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
        let actual = ChatCompletions.request("sarvam-105b", &messages, &[]);

        let expected = json!({
            "model": "sarvam-105b",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}"}
                }]},
                {"role": "tool", "content": "fn main() {}", "tool_call_id": "call-1"},
                {"role": "assistant", "content": "done"},
            ]
        });
        assert_eq!(actual, expected);
    }

    #[test]
    fn nests_the_tool_spec_under_function() {
        let body = ChatCompletions.request("sarvam-105b", &[], &[spec()]);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn omits_tools_when_there_are_none() {
        let body = ChatCompletions.request("sarvam-105b", &[], &[]);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn parses_text_response() {
        let body = json!({
            "choices": [{
                "finish_reason": "stop",
                "index": 0,
                "message": {"role": "assistant", "content": "hello", "tool_calls": null}
            }]
        })
        .to_string();
        let completion = ChatCompletions.normalize(&body).unwrap();
        assert_eq!(completion.finish_reason, "stop");
        assert_eq!(
            completion.message,
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
        })
        .to_string();
        let completion = ChatCompletions.normalize(&body).unwrap();
        assert_eq!(
            completion.message,
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

    #[test]
    fn reports_a_response_with_no_choices() {
        let body = json!({"choices": []}).to_string();
        assert!(matches!(
            ChatCompletions.normalize(&body),
            Err(LlmError::EmptyResponse)
        ));
    }
}
