//! The OpenAI-shaped chat completions wire: `POST /v1/chat/completions`.
//!
//! Sarvam speaks this, as does any OpenAI-compatible endpoint. One flat
//! assistant message carries content and tool calls as sibling fields, so the
//! order between them is not expressible; that is the format's limit, not ours.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Completion, LlmError, StreamState, Transport};
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

    fn on_event(&self, data: &str, state: &mut StreamState) -> Result<Option<String>, LlmError> {
        let chunk: Chunk = serde_json::from_str(data).map_err(|source| LlmError::Decode {
            source,
            body: data.to_string(),
        })?;
        let Some(choice) = chunk.choices.into_iter().next() else {
            return Ok(None);
        };
        if let Some(reason) = choice.finish_reason {
            state.finish_reason = reason;
        }
        let Some(delta) = choice.delta else {
            return Ok(None);
        };

        for call in delta.tool_calls.unwrap_or_default() {
            let function = call.function.unwrap_or(ChunkFunction {
                name: None,
                arguments: None,
            });
            state.merge_tool_call(
                call.index,
                call.id.as_deref(),
                function.name.as_deref(),
                function.arguments.as_deref(),
            );
        }

        // Only text is shown as it arrives. A half-built tool call is not
        // something the user can read, and showing it would put JSON in the
        // middle of an answer.
        Ok(match delta.content {
            Some(text) if !text.is_empty() => {
                state.content.push_str(&text);
                Some(text)
            }
            _ => None,
        })
    }

    fn finish(&self, state: StreamState) -> Result<Completion, LlmError> {
        // This wire sends nothing but deltas, so the turn is whatever they
        // added up to.
        Ok(state.into_completion())
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

/// Chunks carry a fragment of one choice. Content and tool calls arrive as
/// separate slivers, and a tool call is keyed by `index` rather than by id -
/// the id itself is sent in pieces too - so position is what joins them.
#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: Option<ChunkDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Deserialize)]
struct ChunkToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChunkFunction>,
}

#[derive(Deserialize)]
struct ChunkFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
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

    fn feed(chunks: &[Value]) -> (String, Completion) {
        let mut state = StreamState::default();
        let mut shown = String::new();
        for chunk in chunks {
            if let Some(text) = ChatCompletions
                .on_event(&chunk.to_string(), &mut state)
                .unwrap()
            {
                shown.push_str(&text);
            }
        }
        (shown, ChatCompletions.finish(state).unwrap())
    }

    #[test]
    fn assembles_text_from_deltas() {
        let (shown, completion) = feed(&[
            json!({"choices":[{"delta":{"content":"Hel"}}]}),
            json!({"choices":[{"delta":{"content":"lo"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        ]);
        assert_eq!(shown, "Hello");
        assert_eq!(completion.finish_reason, "stop");
        assert_eq!(
            completion.message,
            Message::Assistant {
                content: Some("Hello".into()),
                tool_calls: vec![],
            }
        );
    }

    #[test]
    fn assembles_a_tool_call_split_across_chunks() {
        // The id, the name and the arguments all arrive in pieces, keyed by
        // index rather than by id, so position is what joins them.
        let (shown, completion) = feed(&[
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-","function":{"name":"read_"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"1","function":{"name":"file","arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        assert!(shown.is_empty(), "a half-built call must not be shown");
        assert_eq!(
            completion.message,
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                }],
            }
        );
    }

    #[test]
    fn assembles_two_parallel_tool_calls() {
        let (_, completion) = feed(&[
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"terminal","arguments":"{}"}}]}}]}),
        ]);
        let Message::Assistant { tool_calls, .. } = completion.message else {
            panic!("expected an assistant turn");
        };
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[1].name, "terminal");
    }

    #[test]
    fn a_chunk_with_no_choices_is_not_an_error() {
        // Some providers open a stream with a metadata-only chunk.
        let mut state = StreamState::default();
        assert!(
            ChatCompletions
                .on_event(&json!({"choices":[]}).to_string(), &mut state)
                .unwrap()
                .is_none()
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
