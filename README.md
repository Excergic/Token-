# token

A local CLI that takes a task, calls tools, and returns one answer. You approve every change before it happens.

Today it is a coding agent for the repository you run it in. It can read files, write files, and run shell commands. OpenAI is the default model. Sarvam works with `--provider sarvam`.

The longer aim is the same loop for other work: market research, go-to-market drafts, and lead lists, plus MCP tools you add yourself. That is written in `plan.md`. It is not built yet. This binary does not search the web, place trades, send email, or post anywhere.

## Run

```sh
cp .env.example .env   # put a real key in it
cargo run -- "Explain src/main.rs"
cargo run -- --tui
cargo test
```

Runs are stored in `~/.token/sessions.db`, one conversation per directory. `--resume` continues the latest one in this directory.

## What it will not do on its own

- Write or run a command until you pick Allow Once or Allow Always. No answer counts as no.
- Read or write `.env` and other credential files.
- Reach the network from a sandboxed command unless you pass `--allow-network`. On macOS the default sandbox confines writes to the project. Loopback stays open. On other platforms commands are not sandboxed, and the program says so.
