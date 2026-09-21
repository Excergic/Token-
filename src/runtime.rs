use crate::conversation::{Conversation, Message};
use crate::llm::{LlmClient, LlmError};

const SYSTEM_PROMPT: &str = "You are token, a coding agent that works inside the user's \
project directory. You do not have any tools yet, so you cannot read, write or run files. \
If the task needs information you do not have, say exactly what you would need instead of \
guessing.";

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("task is empty")]
    EmptyTask,
    #[error("model requested tool `{name}` with arguments {arguments}, but no tools are available yet")]
    UnsupportedToolCall { name: String, arguments: String },
    #[error("model returned an empty message")]
    EmptyAnswer,
    #[error(transparent)]
    Llm(#[from] LlmError),
}

/// The agent runtime: takes a task, returns a response.
///
/// Owns the conversation. Today it is one round trip: system + user in,
/// assistant out. The tool loop will grow inside `run`.
pub struct AgentRuntime {
    llm: LlmClient,
}

impl AgentRuntime {
    pub fn new(llm: LlmClient) -> Self {
        Self { llm }
    }

    pub fn run(&self, task: &str) -> Result<String, RuntimeError> {
        let task = task.trim();
        if task.is_empty() {
            return Err(RuntimeError::EmptyTask);
        }

        let mut conversation = Conversation::new();
        conversation.push(Message::System(SYSTEM_PROMPT.to_string()));
        conversation.push(Message::User(task.to_string()));

        let reply = self.llm.chat(conversation.messages())?;
        conversation.push(reply.clone());

        match reply {
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let call = &tool_calls[0];
                Err(RuntimeError::UnsupportedToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
            }
            Message::Assistant {
                content: Some(text),
                ..
            } if !text.trim().is_empty() => Ok(text),
            _ => Err(RuntimeError::EmptyAnswer),
        }
    }
}
