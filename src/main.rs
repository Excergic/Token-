mod cli;
mod llm;
mod runtime;

use clap::Parser;
use std::process;

use cli::Cli;
use llm::LlmClient;
use runtime::AgentRuntime;

fn main() {
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
