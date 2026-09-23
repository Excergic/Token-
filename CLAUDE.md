# token

A coding-agent CLI in Rust. One task in, one answer out, with the model able to
call tools. OpenAI is the default provider and Sarvam is supported alongside
it; nothing above `llm/` is provider-specific.

> The `CLAUDE.md` in the parent directory describes a LinkedIn outreach team.
> It does not apply to this project. This file does.

## Run it

```sh
cp .env.example .env        # then put a real key in it
cargo run -- "Explain src/main.rs"                    # openai, gpt-5.5
cargo run -- --provider sarvam "Explain src/main.rs"  # sarvam-105b
cargo run -- --resume "And what calls it?"            # continue this directory
cargo test
```

Every run is recorded to `~/.token/sessions.db`, keyed by the directory it ran
in. `--resume` continues that directory's most recent conversation;
`--no-session` opts out of both reading and writing.

Tool calls and rejections are traced on stderr (`→` a call, `✗` a rejection),
so `2>/dev/null` gives just the answer and `2>&1 >/dev/null` gives just the
trace. Behaviour varies between runs; check the trace before concluding what
the model did.

## Layers

```
main.rs        composition root, nothing else
cli.rs         clap args; knows nothing about the agent
conversation.rs  provider-agnostic Message / ToolCall / Conversation
runtime.rs     owns the Conversation, drives the tool loop, records it
session.rs     SQLite transcripts; the ONLY module that knows the schema
llm/mod.rs     Transport trait, ApiMode ladder, stateless HTTP client
llm/chat.rs    the ONLY module that knows the chat/completions JSON
llm/responses.rs  the ONLY module that knows the Responses JSON
tools.rs       Tool trait, Registry, handlers; know nothing about conversations
policy.rs      what the agent may never read or write; no I/O, pure decisions
secrets.rs     redaction for secrets policy.rs had no chance to refuse
```

Do not collapse these. The runtime owns conversation state deliberately, so the
transcript can be inspected, truncated or replayed later.

## Rules

- **Every tool is a `Tool` impl registered in `Registry`.** The model-visible
  spec comes from the same object that executes, so the two cannot drift. Never
  hand-write a spec beside a separate handler.
- **Tool failures go to the model, not the user.** They come back as a failed
  tool result so it can correct itself. Only runtime-level problems reach the
  user and exit non-zero.
- **Arguments are strict** (`deny_unknown_fields` and `additionalProperties:
  false`). The model invents fields like `offset` when a tool frustrates it; the
  rejection message is what teaches it.
- **Two size tiers.** `MAX_READ_FILE_BYTES` (512 MiB) refuses a read outright;
  the context cap (`--max-tool-output`, default 64 KiB) truncates every tool's
  output at the dispatch boundary with a marker. Truncation backs up to a UTF-8
  boundary. Keep the cap a parameter: this app is not tied to one model's
  context window.
- **The model invents tools it does not have**, roughly 1 run in 3 on write
  requests even with correct grounding. Grounding reduces attempts; the registry
  lookup, strict arguments and the `<tool_call>` content check catch the rest.
  Do not remove a layer because the others exist.
- **One transport per wire format.** A transport is pure: `request` renders our
  types, `normalize` reads the reply back into them, and the runtime always
  gets a `Completion` whichever API answered. Shared concerns (HTTP client,
  timeout, auth, status handling) stay in `LlmClient`. `ApiMode` is inferred
  from the base URL's **host**, never the model name, so an unfamiliar model id
  cannot silently change which API is called. An explicit `--api-mode` wins.
- **Tool specs are provider-neutral.** `Tool::spec()` returns a `ToolSpec`;
  each transport renders it (chat nests under `function`, Responses is flat).
  `tools.rs` must not learn any provider's JSON shape.
- **Sessions are write-through.** The runtime records each message before it
  reaches the `Conversation`, so there is no exit path that can lose a turn and
  no flush to forget. A failed write stops the run rather than continuing
  against a transcript that will not come back.
- **A resumed transcript is repaired before it is replayed.** A run killed
  mid-loop leaves tool calls that were never answered, and both wire formats
  reject that. `repaired_len` cuts back to the last complete turn and the
  repair is written back, so disk and memory never disagree. A resumed session
  keeps its stored system prompt verbatim: rebuilding it would change the
  prompt under a conversation already held with the old one.
- **Nothing is written without the user's approval.** `Tool::mutates` marks a
  tool as changing the filesystem; the registry asks before dispatching one and
  never asks for anything else. `approve` is a parameter, so `tools.rs` stays
  free of terminal I/O and a test can answer for itself. Silence is a no: a
  closed stdin, an unanswered prompt and a read error all decline, because the
  costly mistake is writing a file nobody agreed to. `--yes` skips the asking.
- **Secrets are refused before they are opened, not after.** `policy.rs` denies
  `.env`, key material and credential files to reads and writes alike. Once a
  key reaches the model it has left the machine and is in the session database,
  so the check cannot live in the answer. A private path is refused before the
  approval prompt too: previewing `.env` would print the secret.
- **Blocking `.env` alone leaks.** Told to record a secret, the model writes the
  real value into `.env.example` instead, and that file is committed while
  `.env` is gitignored. `policy::committed_secret` refuses a secret-looking key
  in a shareable dotenv unless its value is a placeholder. Observed live, not
  imagined: the wording in the system prompt did not stop it, the check did.
- **Writes stay inside the project.** Containment is decided lexically, before
  anything is created, or `../elsewhere/f.txt` would make a directory outside
  the project and only then be refused. The root is canonicalised first (macOS
  `/var` and the temp directory are symlinks), and a symlinked target is
  refused rather than followed.
- **Redaction is the backstop, at two boundaries.** `policy.rs` refuses files
  it can name; it cannot name a credential inside `config/settings.json` or a
  log. So the runtime redacts every tool result before it reaches the
  conversation (and therefore before it is sent to the provider on every later
  turn), and `session.rs` redacts every message on its way into SQLite, because
  that file outlives the run. The two are separate on purpose: the second
  catches a secret the user typed, which never passed through a tool.
- **Redaction must not corrupt source code.** This agent reads code all day.
  The shaped patterns (`sk-…`, `AKIA…`, PEM blocks) are safe; the generic
  `key = value` rule is not, so its value must mix letters and digits, which
  keeps `let token = self.next_token()` and `MAX_TOKENS = 4096` intact. A
  missed secret is a risk; a corrupted file is a certainty. Tests pin both
  directions, and redaction is idempotent so a resumed transcript does not
  nest markers.
- **Ask before deciding.** Limits, loop bounds, error handling, prompt wording
  and module placement are the owner's calls, not defaults to pick silently.

## Not built yet (deliberate)

Any exec tool; a file-slice read; an append or partial edit (`write_file`
replaces a file whole); writes outside the project directory; `max_tokens` /
`reasoning_effort` / `max_output_tokens` in the request (their absence causes
intermittent `finish_reason: length` empty answers); carrying OpenAI reasoning
items between turns (needs `encrypted_content` and a place in `Message` for it,
so a reasoning model currently loses that context across a tool loop); the
`anthropic_messages` transport; named sessions (`--session <id>`: the row is
there, cwd is just the only lookup today), listing or searching past sessions,
and transcript compression (nothing prunes a session, so a long-running
directory grows until the context cap bites); a read sandbox (`policy.rs` is a
blocklist, not confinement: anything it does not name is readable, and
`read_file` still reaches outside the project even though `write_file` cannot);
env filtering for a future exec tool (a child process would inherit
`OPENAI_API_KEY` / `SARVAM_API_KEY` from this one, so an exec tool must strip
them before it is shipped); a configurable turn limit (`MAX_TURNS` is 10, and a 12-file task exhausts it).

## Environment note

`.cargo/config.toml` pins `SDKROOT` to the 26.5 SDK: the default macOS 27.0 SDK
on this machine ships `.tbd` stubs the installed linker cannot parse. Remove it
if Command Line Tools are updated.
