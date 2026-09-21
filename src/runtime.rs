use crate::llm::{LlmClient, LlmError};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("task is empty")]
    EmptyTask,
    #[error(transparent)]
    Llm(#[from] LlmError),
}

/// The agent runtime: takes a task, returns a response.
/// Today it forwards the task to the LLM once. Tools and the agent loop
/// will live here.
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
        Ok(self.llm.complete(task)?)
    }
}
