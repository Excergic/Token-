use clap::Parser;

/// A CLI agent
#[derive(Parser, Debug)]
#[command(name = "token", version, about)]
pub struct Cli {
    /// The task to hand to the agent
    #[arg(required = true, num_args = 1.., value_name = "TASK")]
    task: Vec<String>,
}

impl Cli {
    /// The task as a single string, with the shell-split words rejoined.
    pub fn task(&self) -> String {
        self.task.join(" ")
    }
}
