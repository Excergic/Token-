use clap::Parser;
use std::path::PathBuf;

use crate::llm::{ApiMode, Provider};
use crate::tools::{DEFAULT_EXEC_TIMEOUT_SECS, DEFAULT_MAX_TOOL_OUTPUT_BYTES};

/// A CLI coding agent. Talks to OpenAI by default; `--provider sarvam` and any
/// OpenAI-compatible endpoint via `--base-url` also work.
#[derive(Parser, Debug)]
#[command(name = "token", version, about)]
pub struct Cli {
    /// The task to hand to the agent
    #[arg(required = true, num_args = 1.., value_name = "TASK")]
    task: Vec<String>,

    /// Which provider to talk to. Sets the default base URL, model and key.
    #[arg(long, value_enum, default_value_t = Provider::Openai)]
    pub provider: Provider,

    /// Model to use (default: the provider's)
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// API base URL (default: the provider's)
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// Force a wire format instead of deriving it from the base URL's host
    #[arg(long, value_enum, value_name = "MODE")]
    pub api_mode: Option<ApiMode>,

    /// Maximum bytes of a tool's output given to the model. Raise it for
    /// models with a larger context window.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_TOOL_OUTPUT_BYTES)]
    pub max_tool_output: usize,

    /// API key (default: the provider's env var, OPENAI_API_KEY or SARVAM_API_KEY)
    #[arg(long, value_name = "KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Withhold the shell command tool. It is offered by default; with this
    /// set the model is not given it at all, rather than refused on use.
    #[arg(long)]
    pub no_exec: bool,

    /// Seconds a command may run before it is killed
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_EXEC_TIMEOUT_SECS)]
    pub exec_timeout: u64,

    /// Apply file changes without asking. Every write and every command is
    /// approved by the user unless this is set.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Continue this directory's most recent conversation
    #[arg(long)]
    pub resume: bool,

    /// Do not read or write session history for this run
    #[arg(long, conflicts_with = "resume")]
    pub no_session: bool,

    /// Session database (default: ~/.token/sessions.db)
    #[arg(long, value_name = "PATH")]
    session_db: Option<PathBuf>,
}

/// Which variable to set, named explicitly: with two providers, "no API key"
/// on its own leaves the user guessing which one was wanted.
#[derive(Debug, thiserror::Error)]
#[error("no API key for {provider}: pass --api-key or set {variable}")]
pub struct MissingApiKey {
    provider: &'static str,
    variable: &'static str,
}

impl Cli {
    /// The task as a single string, with the shell-split words rejoined.
    pub fn task(&self) -> String {
        self.task.join(" ")
    }

    pub fn model(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| self.provider.default_model().to_string())
    }

    pub fn base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| self.provider.base_url().to_string())
    }

    /// Where the transcripts live. One file for every directory, outside any
    /// project, so nothing has to be gitignored.
    pub fn session_db(&self) -> PathBuf {
        self.session_db.clone().unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".token").join("sessions.db")
        })
    }

    /// The flag wins, then the provider's own variable. Not resolved by clap's
    /// `env`, which takes one fixed name and cannot follow `--provider`.
    pub fn api_key(&self) -> Result<String, MissingApiKey> {
        if let Some(key) = &self.api_key {
            return Ok(key.clone());
        }
        let variable = self.provider.key_env();
        std::env::var(variable).map_err(|_| MissingApiKey {
            provider: match self.provider {
                Provider::Openai => "openai",
                Provider::Sarvam => "sarvam",
            },
            variable,
        })
    }
}
