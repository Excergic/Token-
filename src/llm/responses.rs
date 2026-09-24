//! The OpenAI Responses wire: `POST /v1/responses`.
//!
//! Input and output are ordered lists of typed items rather than one flat
//! message, so a turn that reasoned, called a tool and then spoke keeps that
//! order. We send `store: false` and replay the whole transcript ourselves:
//! the runtime owns the conversation here exactly as it does for chat.
//!
//! Two details this format will not forgive:
//!
//! - A `function_call` is answered by a `function_call_output` carrying the
//!   same `call_id`. The item's own `id` is a different value and echoing it
//!   back is rejected, so `ToolCall::id` holds the `call_id`.
//! - Reasoning items are dropped rather than replayed. Carrying them needs
//!   `encrypted_content` and a place in `Message` to put it, which is its own
//!   change; until then a reasoning model loses that context between turns.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Completion, LlmError, StreamState, Transport};
use crate::conversation::{Message, ToolCall};
use crate::tools::ToolSpec;

pub(super) struct OpenAiResponses;

impl Transport for OpenAiResponses {
    fn path(&self) -> &'static str {
        "/v1/responses"
    }

    fn request(&self, model: &str, messages: &[Message], tools: &[ToolSpec]) -> Value {
        let mut input: Vec<Value> = Vec::new();
        for message in messages {
            push_items(message, &mut input);
        }

        let mut body = json!({
            "model": model,
            "input": input,
            // No server-side state: the transcript is ours to send.
            "store": false,
        });
        if !tools.is_empty() {
            body["tools"] = tools.iter().map(tool_json).collect();
        }
        body
    }

    fn on_event(&self, data: &str, state: &mut StreamState) -> Result<Option<String>, LlmError> {
        let event: StreamEvent = serde_json::from_str(data).map_err(|source| LlmError::Decode {
            source,
            body: data.to_string(),
        })?;

        match event.kind.as_str() {
            "response.output_text.delta" => {
                let text = event.delta.unwrap_or_default();
                state.content.push_str(&text);
                Ok((!text.is_empty()).then_some(text))
            }
            // This wire ends by sending the finished response whole, so the
            // turn is parsed by the same code a non-streamed one goes
            // through rather than reassembled from the pieces.
            "response.completed" | "response.incomplete" | "response.failed" => {
                let Some(response) = event.response else {
                    return Ok(None);
                };
                state.completion = Some(self.normalize(&response.to_string())?);
                Ok(None)
            }
            // Reasoning summaries, item lifecycle, argument deltas: nothing
            // the reader can use mid-answer.
            _ => Ok(None),
        }
    }

    fn finish(&self, state: StreamState) -> Result<Completion, LlmError> {
        state.completion.ok_or(LlmError::EmptyResponse)
    }

    fn normalize(&self, body: &str) -> Result<Completion, LlmError> {
        let parsed: ResponsesBody =
            serde_json::from_str(body).map_err(|source| LlmError::Decode {
                source,
                body: body.to_string(),
            })?;

        if parsed.output.is_empty() {
            return Err(LlmError::EmptyResponse);
        }

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for item in parsed.output {
            match item.kind.as_str() {
                "message" => {
                    for part in item.content.unwrap_or_default() {
                        if part.kind == "output_text" {
                            if let Some(chunk) = part.text {
                                text.push_str(&chunk);
                            }
                        }
                    }
                }
                "function_call" => {
                    // A call missing any of the three is unusable; skipping it
                    // leaves the turn looking like plain text, which the
                    // runtime already handles.
                    if let (Some(id), Some(name), Some(arguments)) =
                        (item.call_id, item.name, item.arguments)
                    {
                        tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments,
                        });
                    }
                }
                // Reasoning and hosted-tool items: nothing to replay.
                _ => {}
            }
        }

        Ok(Completion {
            message: Message::Assistant {
                content: (!text.is_empty()).then_some(text),
                tool_calls,
            },
            finish_reason: finish_reason(parsed.status, parsed.incomplete_details),
        })
    }
}

/// Our transcript, as input items. An assistant turn can become two items,
/// which is the point of the format.
fn push_items(message: &Message, input: &mut Vec<Value>) {
    match message {
        Message::System(content) => input.push(json!({"role": "system", "content": content})),
        Message::User(content) => input.push(json!({"role": "user", "content": content})),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            // An assistant item with empty content is rejected, and a turn
            // that only called a tool legitimately has none.
            if let Some(text) = content.as_deref().filter(|text| !text.trim().is_empty()) {
                input.push(json!({"role": "assistant", "content": text}));
            }
            for call in tool_calls {
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments,
                }));
            }
        }
        Message::Tool {
            tool_call_id,
            content,
        } => input.push(json!({
            "type": "function_call_output",
            "call_id": tool_call_id,
            "output": content,
        })),
    }
}

/// Flat, unlike the chat wire. `strict` is set because our argument structs
/// already reject unknown fields; the two halves of that rule should agree.
fn tool_json(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": spec.name,
        "description": spec.description,
        "parameters": spec.parameters,
        "strict": true,
    })
}

/// Flattened into the same vocabulary the chat wire uses, so the runtime's
/// error messages mean one thing regardless of which API answered.
fn finish_reason(status: Option<String>, incomplete: Option<IncompleteDetails>) -> String {
    match status.as_deref() {
        Some("completed") => "stop".to_string(),
        Some("incomplete") => match incomplete.and_then(|details| details.reason) {
            Some(reason) if reason == "max_output_tokens" => "length".to_string(),
            Some(reason) => reason,
            None => "incomplete".to_string(),
        },
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// One named event from the stream. Only the few fields that matter here are
/// read; the wire sends many kinds and most are framing.
#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    response: Option<Value>,
}

#[derive(Deserialize)]
struct ResponsesBody {
    #[serde(default)]
    output: Vec<OutputItem>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct OutputItem {
    #[serde(rename = "type")]
    kind: String,
    /// `Option`, not a defaulted `Vec`: a provider that sends an explicit
    /// `null` here would otherwise fail to decode with an opaque error.
    #[serde(default)]
    content: Option<Vec<ContentPart>>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ContentPart {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
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
    fn maps_every_message_variant_to_items() {
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
        let body = OpenAiResponses.request("gpt-5.5", &messages, &[]);

        assert_eq!(body["store"], false);
        assert_eq!(
            body["input"],
            json!([
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"},
                {
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "read_file",
                    "arguments": "{\"path\":\"src/main.rs\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call-1",
                    "output": "fn main() {}"
                },
                {"role": "assistant", "content": "done"},
            ])
        );
    }

    #[test]
    fn an_assistant_turn_with_text_and_a_call_becomes_two_items() {
        let messages = [Message::Assistant {
            content: Some("reading it now".into()),
            tool_calls: vec![ToolCall {
                id: "call-9".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        }];
        let body = OpenAiResponses.request("gpt-5.5", &messages, &[]);
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert_eq!(body["input"][0]["role"], "assistant");
        assert_eq!(body["input"][1]["type"], "function_call");
    }

    #[test]
    fn keeps_the_tool_spec_flat() {
        let body = OpenAiResponses.request("gpt-5.5", &[], &[spec()]);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn parses_text_output() {
        let body = json!({
            "status": "completed",
            "output": [
                {"id": "rs_1", "type": "reasoning", "content": [], "summary": []},
                {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello"}]
                }
            ]
        })
        .to_string();
        let completion = OpenAiResponses.normalize(&body).unwrap();
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
    fn takes_call_id_not_the_item_id() {
        // Echoing `id` back instead of `call_id` is rejected by the API, so
        // this is the difference that matters.
        let body = json!({
            "status": "completed",
            "output": [{
                "id": "fc_68a2",
                "type": "function_call",
                "call_id": "call_abc123",
                "name": "read_file",
                "arguments": "{\"path\":\"src/main.rs\"}"
            }]
        })
        .to_string();
        let completion = OpenAiResponses.normalize(&body).unwrap();
        assert_eq!(
            completion.message,
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_abc123".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"src/main.rs"}"#.into(),
                }],
            }
        );
    }

    #[test]
    fn a_truncated_answer_reports_length() {
        // Same word the chat wire uses, so the runtime's error reads the same.
        let body = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"id": "rs_1", "type": "reasoning", "content": [], "summary": []}]
        })
        .to_string();
        let completion = OpenAiResponses.normalize(&body).unwrap();
        assert_eq!(completion.finish_reason, "length");
        assert_eq!(
            completion.message,
            Message::Assistant {
                content: None,
                tool_calls: vec![],
            }
        );
    }

    #[test]
    fn survives_a_null_content_field() {
        let body = json!({
            "status": "completed",
            "output": [{"id": "rs_1", "type": "reasoning", "content": null}]
        })
        .to_string();
        assert!(OpenAiResponses.normalize(&body).is_ok());
    }

    #[test]
    fn shows_text_deltas_as_they_arrive() {
        let mut state = StreamState::default();
        let shown: String = [
            json!({"type":"response.output_text.delta","delta":"Hel"}),
            json!({"type":"response.output_text.delta","delta":"lo"}),
        ]
        .iter()
        .filter_map(|event| {
            OpenAiResponses
                .on_event(&event.to_string(), &mut state)
                .unwrap()
        })
        .collect();
        assert_eq!(shown, "Hello");
    }

    #[test]
    fn the_finished_turn_comes_from_the_terminal_event() {
        // This wire sends the completed response whole, so the turn goes
        // through the same parser a non-streamed one does.
        let mut state = StreamState::default();
        let done = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": "read_file",
                    "arguments": "{}"
                }]
            }
        });
        assert!(
            OpenAiResponses
                .on_event(&done.to_string(), &mut state)
                .unwrap()
                .is_none()
        );
        let completion = OpenAiResponses.finish(state).unwrap();
        assert_eq!(completion.finish_reason, "stop");
        assert_eq!(
            completion.message,
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_abc".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                }],
            }
        );
    }

    #[test]
    fn a_truncated_stream_reports_length() {
        let mut state = StreamState::default();
        let done = json!({
            "type": "response.incomplete",
            "response": {
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "output": [{"id": "rs_1", "type": "reasoning"}]
            }
        });
        OpenAiResponses
            .on_event(&done.to_string(), &mut state)
            .unwrap();
        assert_eq!(
            OpenAiResponses.finish(state).unwrap().finish_reason,
            "length"
        );
    }

    #[test]
    fn framing_events_are_ignored() {
        let mut state = StreamState::default();
        for kind in [
            "response.created",
            "response.output_item.added",
            "response.reasoning_summary_text.delta",
        ] {
            assert!(
                OpenAiResponses
                    .on_event(&json!({"type": kind}).to_string(), &mut state)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn a_stream_that_never_completed_is_an_error() {
        // Rather than handing back an empty answer as if it were the turn.
        assert!(matches!(
            OpenAiResponses.finish(StreamState::default()),
            Err(LlmError::EmptyResponse)
        ));
    }

    #[test]
    fn reports_an_empty_output() {
        let body = json!({"status": "completed", "output": []}).to_string();
        assert!(matches!(
            OpenAiResponses.normalize(&body),
            Err(LlmError::EmptyResponse)
        ));
    }
}
