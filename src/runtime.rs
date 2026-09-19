use std::fmt;

/// Errors the runtime can produce. Will grow once an LLM is wired in.
#[derive(Debug)]
pub enum RuntimeError {
    EmptyTask,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::EmptyTask => write!(f, "task is empty"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// The agent runtime: takes a task, returns a response.
/// Currently a stub; the LLM and tool loop will live here.
pub struct AgentRuntime {
    // model client, tools, config, ...
}

impl AgentRuntime {
    pub fn new() -> Self {
        Self {}
    }

    pub fn run(&self, task: &str) -> Result<String, RuntimeError> {
        let task = task.trim();
        if task.is_empty() {
            return Err(RuntimeError::EmptyTask);
        }
        Ok(format!("Agent received: {task}"))
    }
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::new()
    }
}
