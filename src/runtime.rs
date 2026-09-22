use std::path::PathBuf;

use crate::conversation::{Conversation, Message};
use crate::llm::{LlmClient, LlmError};
use crate::tools;

const SYSTEM_PROMPT: &str = "You are token, a coding agent that works inside the user's \
project directory. Use the read_file tool to look at files instead of guessing or asking \
the user to paste them. Paths are relative to the project root, for example src/main.rs. \
When you have what you need, answer directly.";

/// Most assistant turns one run may take before giving up.
const MAX_TURNS: usize = 10;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("task is empty")]
    EmptyTask,
    #[error("model returned an empty message (finish_reason: {0})")]
    EmptyAnswer(String),
    #[error("reached the {MAX_TURNS} turn limit without a final answer")]
    TurnLimit,
    #[error(transparent)]
    Llm(#[from] LlmError),
}

/// The agent runtime: takes a task, returns a response.
///
/// Owns the conversation and drives the loop:
/// user -> assistant tool_call -> tool result -> assistant ... -> final answer.
pub struct AgentRuntime {
    llm: LlmClient,
    root: PathBuf,
}

impl AgentRuntime {
    pub fn new(llm: LlmClient, root: PathBuf) -> Self {
        Self { llm, root }
    }

    pub fn run(&self, task: &str) -> Result<String, RuntimeError> {
        let task = task.trim();
        if task.is_empty() {
            return Err(RuntimeError::EmptyTask);
        }

        let tools = tools::definitions();
        let mut conversation = Conversation::new();
        conversation.push(Message::System(SYSTEM_PROMPT.to_string()));
        conversation.push(Message::User(task.to_string()));

        for _ in 0..MAX_TURNS {
            let completion = self.llm.chat(conversation.messages(), &tools)?;
            conversation.push(completion.message.clone());

            let Message::Assistant {
                content,
                tool_calls,
            } = completion.message
            else {
                return Err(RuntimeError::EmptyAnswer(completion.finish_reason));
            };

            // No tool requested: this is the final answer.
            if tool_calls.is_empty() {
                return match content {
                    Some(text) if !text.trim().is_empty() => Ok(text),
                    _ => Err(RuntimeError::EmptyAnswer(completion.finish_reason)),
                };
            }

            // Run every requested tool and feed each result back. A failing
            // tool is reported to the model, not to the user: it can retry
            // with a corrected path.
            for call in tool_calls {
                eprintln!("→ {} {}", call.name, call.arguments);
                let result = match tools::run(&call.name, &call.arguments, &self.root) {
                    Ok(output) => output,
                    Err(err) => format!("error: {err}"),
                };
                conversation.push(Message::Tool {
                    tool_call_id: call.id,
                    content: result,
                });
            }
        }

        Err(RuntimeError::TurnLimit)
    }
}
