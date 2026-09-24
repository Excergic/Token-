mod cli;
mod conversation;
mod llm;
mod policy;
mod runtime;
mod sandbox;
mod secrets;
mod session;
mod tools;

use clap::Parser;
use std::error::Error;
use std::process;
use std::time::Duration;

use cli::Cli;
use llm::{LlmClient, resolve_api_mode};
use runtime::AgentRuntime;
use sandbox::SandboxPolicy;
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

    let mut runtime =
        AgentRuntime::new(llm, root.clone(), cli.max_tool_output).with_auto_approve(cli.yes);
    if !cli.no_exec {
        let backend = sandbox::select(cli.sandbox);
        if backend == sandbox::Backend::None && cli.sandbox != sandbox::SandboxMode::Off {
            // Never let the user believe in a boundary that is not there.
            eprintln!("! no sandbox available on this platform; commands run unconfined");
        }
        runtime = runtime.with_exec(
            Duration::from_secs(cli.exec_timeout),
            backend,
            SandboxPolicy::new(cli.sandbox, &root, cli.allow_network),
        );
    }
    if !cli.no_session {
        runtime = runtime.with_sessions(SessionStore::open(&cli.session_db())?, cli.resume);
    }

    Ok(runtime.run(&cli.task())?)
}
