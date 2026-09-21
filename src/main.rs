mod cli;
mod conversation;
mod llm;
mod runtime;

use clap::Parser;
use std::process;

use cli::Cli;
use llm::LlmClient;
use runtime::AgentRuntime;

fn main() {
    // Load .env (if any) before clap reads SARVAM_API_KEY from the environment.
    // A real env var set in the shell still wins over the file.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    let llm = LlmClient::new(&cli.api_key, &cli.model);
    let runtime = AgentRuntime::new(llm);

    match runtime.run(&cli.task()) {
        Ok(response) => println!("{response}"),
        Err(err) => {
            eprintln!("error: {err}");
            process::exit(1);
        }
    }
}
