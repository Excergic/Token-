mod cli;
mod runtime;

use clap::Parser;
use cli::Cli;
use runtime::AgentRuntime;

fn main() {
    let cli = Cli::parse();

    let runtime = AgentRuntime::new();
    
    match runtime.run(&cli.task()){
        Ok(response) => println!("{response}"),
        Err(error) => {
            eprintln!("Error: {error}");
            std::process::exit(1);
        }
    }
}
