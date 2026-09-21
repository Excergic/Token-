use clap::Parser;

use crate::llm::DEFAULT_MODEL;

/// A CLI agent backed by the Sarvam AI chat API.
#[derive(Parser, Debug)]
#[command(name = "token", version, about)]
pub struct Cli {
    /// The task to hand to the agent
    #[arg(required = true, num_args = 1.., value_name = "TASK")]
    task: Vec<String>,

    /// Model to use
    #[arg(long, default_value = DEFAULT_MODEL)]
    pub model: String,

    /// Sarvam API key (falls back to the SARVAM_API_KEY env var)
    #[arg(long, env = "SARVAM_API_KEY", hide_env_values = true)]
    pub api_key: String,
}

impl Cli {
    /// The task as a single string, with the shell-split words rejoined.
    pub fn task(&self) -> String {
        self.task.join(" ")
    }
}
