use std::path::PathBuf;

use crate::conversation::{Conversation, Message};
use crate::llm::{LlmClient, LlmError};
use crate::tools::Registry;

/// Most assistant turns one run may take before giving up.
const MAX_TURNS: usize = 10;

/// How many times the model may be corrected for faking a tool call before
/// the run is abandoned.
const MAX_CORRECTIONS: usize = 2;

/// Sent when the model writes tool markup into its message instead of making
/// a real tool call.
const CORRECTION: &str = "That was not a tool call. Do not write tool markup in your \
message text: it is never executed. Either call a tool through the tool-calling \
mechanism, or answer in plain text. The tools that exist are: ";

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("task is empty")]
    EmptyTask,
    #[error("model returned an empty message (finish_reason: {0})")]
    EmptyAnswer(String),
    #[error("reached the {MAX_TURNS} turn limit without a final answer")]
    TurnLimit,
    #[error("model kept writing fake tool markup instead of calling a tool")]
    FakeToolCalls,
    #[error(transparent)]
    Llm(#[from] LlmError),
}

/// The agent runtime: takes a task, returns a response.
///
/// Owns the conversation and drives the loop:
/// user -> assistant tool_call -> tool result -> assistant ... -> final answer.
pub struct AgentRuntime {
    llm: LlmClient,
    tools: Registry,
    root: PathBuf,
}

impl AgentRuntime {
    pub fn new(llm: LlmClient, root: PathBuf, max_tool_output: usize) -> Self {
        Self {
            llm,
            tools: Registry::new(max_tool_output),
            root,
        }
    }

    pub fn run(&self, task: &str) -> Result<String, RuntimeError> {
        let task = task.trim();
        if task.is_empty() {
            return Err(RuntimeError::EmptyTask);
        }

        let specs = self.tools.specs();
        let mut conversation = Conversation::new();
        conversation.push(Message::System(self.system_prompt()));
        conversation.push(Message::User(task.to_string()));

        let mut corrections = 0;
        for _ in 0..MAX_TURNS {
            let completion = self.llm.chat(conversation.messages(), &specs)?;
            conversation.push(completion.message.clone());

            let Message::Assistant {
                content,
                tool_calls,
            } = completion.message
            else {
                return Err(RuntimeError::EmptyAnswer(completion.finish_reason));
            };

            if tool_calls.is_empty() {
                let text = content.unwrap_or_default();

                // A registry cannot catch this: the model wrote tool markup as
                // prose, so no tool call was ever made. Reject it and say why.
                if looks_like_fake_tool_call(&text) {
                    corrections += 1;
                    if corrections > MAX_CORRECTIONS {
                        return Err(RuntimeError::FakeToolCalls);
                    }
                    eprintln!("✗ ignored invented tool call in message text");
                    conversation
                        .push(Message::User(format!("{CORRECTION}{}.", self.tools.names())));
                    continue;
                }

                return match text.trim().is_empty() {
                    false => Ok(text),
                    true => Err(RuntimeError::EmptyAnswer(completion.finish_reason)),
                };
            }

            // Run every requested tool and feed each result back. A failure is
            // reported to the model, not to the user: it can correct itself.
            for call in tool_calls {
                eprintln!("→ {} {}", call.name, call.arguments);
                let result = match self.tools.dispatch(&call.name, &call.arguments, &self.root) {
                    Ok(output) => output,
                    Err(err) => {
                        eprintln!("  ✗ {err}");
                        format!("error: {err}")
                    }
                };
                conversation.push(Message::Tool {
                    tool_call_id: call.id,
                    content: result,
                });
            }
        }

        Err(RuntimeError::TurnLimit)
    }

    /// The tool names come from the registry, so the prompt cannot advertise a
    /// tool that does not exist.
    fn system_prompt(&self) -> String {
        format!(
            "You are token, a coding agent that works inside the user's project directory. \
The only tools that exist are: {}. Call them through the tool-calling mechanism; never \
write tool markup such as <tool_call> in your message text, and never assume a tool you \
have not been given. Use read_file to look at files instead of guessing or asking the \
user to paste them. Paths are relative to the project root, for example src/main.rs. \
When you have what you need, answer directly.",
            self.tools.names()
        )
    }
}

/// Markup a model emits when it imagines a tool it was never given. Matched
/// against real output seen from this model, not invented patterns.
fn looks_like_fake_tool_call(text: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "<tool_call>",
        "<function_call>",
        "<arg_key>",
        "<invoke name=",
        "<invoke",
    ];
    let text = text.to_ascii_lowercase();
    MARKERS.iter().any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_markup_this_model_actually_emitted() {
        let seen = "<tool_call>terminal\n<arg_key>command</arg_key>\n\
<arg_value>file target/debug/token</arg_value>\n</tool_call>";
        assert!(looks_like_fake_tool_call(seen));
    }

    #[test]
    fn detects_markup_in_other_casings_and_formats() {
        assert!(looks_like_fake_tool_call("sure!\n<FUNCTION_CALL>bash</FUNCTION_CALL>"));
        assert!(looks_like_fake_tool_call("<invoke name=\"terminal\">"));
    }

    #[test]
    fn leaves_ordinary_answers_alone() {
        assert!(!looks_like_fake_tool_call(
            "The runtime calls tools via the tool_calls field; see src/runtime.rs."
        ));
    }

    #[test]
    fn leaves_code_about_tool_calls_alone() {
        // Explaining our own code must not be mistaken for faking a call.
        assert!(!looks_like_fake_tool_call(
            "```rust\nfor call in tool_calls {\n    self.tools.dispatch(&call.name)\n}\n```"
        ));
    }
}
