mod cli;
mod conversation;
mod llm;
mod policy;
mod runtime;
mod secrets;
mod session;
mod tools;

use clap::Parser;
use std::error::Error;
use std::process;

use cli::Cli;
use llm::{LlmClient, resolve_api_mode};
use runtime::AgentRuntime;
use session::SessionStore;

fn main() {
    // Load .env (if any) before the provider's key is read from the
    // environment. A real env var set in the shell still wins over the file.
    let _ = dotenvy::dotenv();

    match run(&Cli::parse()) {
        Ok(response) => println!("{response}"),
        Err(err) => {
            eprintln!("error: {err}");
            process::exit(1);
        }
    }
}

/// Composition root: resolve what to talk to, wire it up, run one task.
fn run(cli: &Cli) -> Result<String, Box<dyn Error>> {
    let base_url = cli.base_url();
    let api_mode = resolve_api_mode(cli.api_mode, &base_url);
    let llm = LlmClient::new(base_url, cli.api_key()?, cli.model(), api_mode);
    let root = std::env::current_dir()?;

    let mut runtime = AgentRuntime::new(llm, root, cli.max_tool_output).with_auto_approve(cli.yes);
    if !cli.no_session {
        runtime = runtime.with_sessions(SessionStore::open(&cli.session_db())?, cli.resume);
    }

    Ok(runtime.run(&cli.task())?)
}
