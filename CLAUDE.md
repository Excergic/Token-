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
cargo run -- "Run the tests and tell me what fails"  # asks before each command
cargo run -- --no-exec "Explain src/runtime.rs"      # no shell tool at all
cargo run -- --sandbox off --allow-network "..."     # unconfined, network on
cargo run -- --tui                                   # interactive screen
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
               read_file, write_file, terminal all live behind that trait
policy.rs      what the agent may never read or write; no I/O, pure decisions
secrets.rs     redaction for secrets policy.rs had no chance to refuse
sandbox.rs     OS-enforced confinement; the ONLY module that knows Seatbelt
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
  closed stdin, an unanswered prompt, a read error and an answer that is not
  recognised all decline, because the costly mistake is acting on consent the
  user did not give. `--yes` skips the asking for every tool.
- **The prompt is a numbered menu, spelled out.** `1) Allow Once`,
  `2) Allow Always`, `3) No` - not bare letters, because `[y/a/N]` makes the
  reader guess which letter does the irreversible thing. Typing the words works
  as well as the numbers. "Allow Always" covers every change for the rest of
  the run and is a plain `bool` owned by `run()`, so it dies with the process
  and never reaches the session database: standing consent that outlived the
  task would be consent the user cannot see or revoke. On a flagged call
  option 2 keeps its number and says it is unavailable rather than vanishing,
  because a menu that changes shape between prompts is one people misread.
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
- **The command tool is on by default; approval is the gate.** A coding agent
  that cannot run the tests is half a tool, so `terminal` is offered unless
  `--no-exec` withholds it. It is named `terminal` because that is the tool the
  model kept inventing before it existed, `command` argument and all: matching
  the name it reaches for turns hallucinated calls into real ones. The per-command prompt is what protects the user,
  not the absence of the tool. When it is withheld it leaves the spec entirely
  rather than being refused on use: a tool the model cannot see is one it does
  not keep trying, or work around.
- **A child process inherits nothing that looks like a credential.**
  `policy::scrub_env` starts from `env_clear()` and adds back only names
  carrying none of KEY / SECRET / TOKEN / PASSWORD / AUTH / CREDENTIAL /
  PRIVATE. Denying reads of `.env` is pointless if `printenv` hands the value
  back. A block list, not an allow list, so `PATH` and `HOME` survive and the
  shell still works.
- **A command gets its own process group and is killed as one.** `Child::kill`
  would end the shell and leave what it spawned running, holding the pipe open
  and hanging the read. stdout and stderr are drained on threads, because a
  command that fills the pipe buffer would otherwise block forever; stdin is
  `/dev/null`, so nothing can sit waiting for input that will never come.
- **A shell goes around every path rule, so the command text is read.**
  `terminal` can `rm .env` or `cat /etc/hosts`; neither is a write the
  registry can refuse. `policy::command_concerns` names what a command touches
  that is private or outside the project, and those reasons are shown in the
  approval prompt. **This is string matching, not a boundary**: `cat .e""nv`,
  `$HOME/.ssh/id_rsa` and `eval` defeat it. It exists to stop an accident and
  must never be described as containment.
- **Neither `--yes` nor "Allow Always" covers a command carrying a concern.** They
  approve the routine case; a command naming a secret or leaving the project is the one the
  user meant to see, so it is always asked. That makes a scripted run fail
  closed on exactly those commands, which is the intended trade. Concerns ride
  on `Approval`, so the decision about what may be waved through lives with the
  request rather than in the prompt.
- **Flagging must stay rare to stay meaningful.** `cargo test`, `ls src`,
  `rm -rf target` and `rustc hello.rs && ./hello` carry no concern. A prompt
  that warns about everything teaches the user to approve without reading, so
  tests pin the quiet cases as firmly as the loud ones.
- **The sandbox is the only real boundary; everything else is a check.**
  `policy.rs` reads a command's text and a quoted or variable-built path walks
  past it. `sandbox.rs` asks the kernel, so `cat ".e""nv"` fails whatever it
  looks like. Modes: `workspace-write` (default) confines writes to the project
  and TMPDIR, `read-only` forbids writes, `off` removes it. Remote network is
  denied unless `--allow-network`, because the network is how anything the
  agent read leaves the machine. Loopback stays open either way, so a command
  can bind and call `127.0.0.1` without a path off the machine.
- **A sandbox that breaks the toolchain gets switched off, and one that is off
  protects nothing.** Reads stay broadly allowed - a compiler needs the SDK and
  half of `/usr` - and only credentials are denied. `.git` stays readable here
  even though `policy.rs` denies it to `read_file`, because denying it breaks
  `git status` and cargo's vcs lookup. The writable temp grant is this
  process's own TMPDIR, never all of `/private/var/folders`: that tree holds
  every application's temp space, and a test escaped through it when the whole
  tree was granted.
- **Seatbelt rule order is the mechanism.** A later rule overrides an earlier
  one, which is how `.env.example` is allowed back after dotenv reads are
  denied wholesale. Paths are quoted when rendered, or one containing a quote
  would end the string early and change the policy's meaning.
- **Never imply a boundary that is not there.** `select` returns `Backend::None`
  on a platform with no support, and the composition root says so out loud
  rather than leaving the user to assume confinement. The sandbox tests stand
  down on such a platform instead of passing vacuously.
- **A tool result is never empty on the wire.** An empty file or a silent
  command produces no output, and at least one provider rejects an empty tool
  message with a 400 that ends the run. The runtime substitutes words.
- **Streaming is about when the user sees an answer, not who owns it.**
  `send_streaming` hands each piece of text to a callback and still returns one
  `Completion`, so the runtime, the conversation and the session are unchanged.
  It is used only when a `TurnSink` is present: the CLI prints one answer at
  the end and takes the simpler path.
- **Each wire reassembles its own stream.** The chat wire sends nothing but
  deltas, so the turn is whatever they add up to - and a tool call arrives in
  pieces keyed by `index`, with the id itself split across chunks, so position
  is what joins them. The Responses wire ends by sending the finished response
  whole, so `response.completed` goes through the same `normalize` a
  non-streamed turn does rather than being rebuilt from fragments.
- **Only text is streamed to the screen.** A half-built tool call is not
  something anyone can read, and showing it would put JSON in the middle of an
  answer. The TUI renders streamed text plain and lets the finished cell do the
  markdown: re-laying out on every delta reflows tables and code blocks as they
  grow, which reads worse than waiting.
- **Null is not absence on this wire either.** Sarvam sends `"content": null`
  and `"tool_calls": null` in most chunks, and closes with a usage-only chunk
  whose `choices` is empty, then `[DONE]`. `Option` fields handle that; a
  defaulted `Vec` would not, which is the trap the non-streaming client already
  fell into once.
- **A non-zero exit is an answer, not a tool failure.** A failing `cargo test`
  is what the model asked to see, so the status and output come back as a
  result. Only a timeout or a failure to spawn is an error.
- **Ask before deciding.** Limits, loop bounds, error handling, prompt wording
  and module placement are the owner's calls, not defaults to pick silently.

## Not built yet (deliberate)

A file-slice read; an append or partial edit (`write_file`
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
a Linux or Windows sandbox (`Backend` is an
enum and the policy is built separately from the rendering, so bubblewrap and
Landlock would slot in, but only macOS Seatbelt exists today and elsewhere
commands run unconfined); running this project's own test suite through the
agent (`sandbox-exec` cannot nest, so the suite's sandbox tests fail inside the
agent's sandbox: use `--sandbox off` for that one case); a persistent kernel, so nothing carries between
commands and each starts fresh; Windows support for `terminal` (it assumes
`/bin/sh` and POSIX process groups); a configurable turn limit (`MAX_TURNS` is 10, and a 12-file task exhausts it).

## Capability map

The paths below are this crate's modules. A Codex-shaped tree (`core/src/`,
`tui/src/`) is not the layout, and these layers stay separate.

| Area | Where it lives | State |
|---|---|---|
| Turn loop | `runtime.rs` `run` | Done. Observe and act through tool calls. No separate plan step. `MAX_TURNS` is 10 and the stop message does not say what already happened. |
| Tool routing | `tools.rs` `Registry` | Done for `read_file`, `write_file`, `terminal`. Missing `list_files`, `grep`, `apply_patch`. A failed tool result already goes back to the model, which is the self-correction that exists today. |
| Context building | `runtime.rs` system prompt, `conversation.rs` | History and the system prompt only. No assembled file contents, diagnostics, or repo map. |
| Context window | `--max-tool-output` in `runtime.rs` | Per-tool truncation only. No token budget and no priority-based inclusion. |
| Session persistence | `session.rs` | Write-through SQLite, `--resume` by cwd, repair of a killed mid-loop transcript. No named session, list, or search. Nothing prunes history. |
| Model client | `llm/` | OpenAI chat, OpenAI Responses, Sarvam via the same transports. No Anthropic, no streaming, no `max_tokens` / `reasoning_effort` / `max_output_tokens`, no reasoning items carried across a tool loop. |
| Prompt templates | `format!` in `runtime.rs` | One string that grew a clause per feature. Not a file that can be diffed and tested on its own. |
| Task decomposition | — | Not built. The model picks one tool at a time. |
| Sub-agents | — | Not built. This would turn one loop into an orchestrator. |
| Learned allowlists | `Approval` in `tools.rs` | "Allow Always" is a `bool` that dies with the process, on purpose. A persisted allowlist needs review and revoke before it exists. |
| Guardian review | approval prompt on stdin | Blocking, in the loop. No async handoff. |
| TUI | `src/tui/`, `--tui` | Interactive screen on branch `TUI-UX`. One-shot CLI stays the default. No model-token streaming yet; the live cell is the in-flight tool. |
| Evaluation | `cargo test` in each module | Unit tests only. No end-to-end harness, no pass@k, latency, or token totals. |

## Next, one at a time

One item per branch, finished and verified before the next starts. Live bugs
first, then things already half-built, then new capability, then the items
that change what this project is. Limits, loop bounds, error handling, prompt
wording and module placement stay the owner's call.

1. **Screening false positives.** Done, in the working tree. `command_concerns`
   believes an absolute token only with two or more components, or one that
   exists on disk. A bare `/` is not flagged. The sandbox still refuses
   `rm -rf /`.
2. **Loopback in the sandbox.** Done. With remote network denied, the Seatbelt
   profile still allows bind, inbound and outbound to `localhost` (127.0.0.1
   and ::1). `--allow-network` still opens the real internet, and that profile
   does not also carry the denial.
2b. **Streaming.** Done. Both transports parse SSE and `TurnEvent::Delta`
   flows through the existing `TurnSink` into the widget; verified against both
   live wires. Left open: Sarvam streams its reasoning trace as
   `reasoning_content` deltas, which is why nothing appears for several seconds
   before an answer - 671 of 675 tokens on a "say hello" turn - and showing it
   dimmed would fill that wait. There is also no stable/tail split, so a
   markdown table only lays out once the turn finishes.
3. **`max_tokens` / `reasoning_effort` / `max_output_tokens`.** The cause of
   intermittent `finish_reason: length` empty answers. Values and defaults are
   the owner's call.
4. **Turn limit.** Make `MAX_TURNS` a flag. The failure names what was done so
   far, not only that the loop stopped.
5. **More tools.** `list_files` and `grep`, then `apply_patch`. Each is a
   `Tool` impl. `apply_patch` inherits `write_file`'s confinement.
6. **Prompt templates.** Move the system prompt out of the `format!` in
   `runtime.rs` so it can be read, diffed and tested.
7. **Context window management.** Token budgeting and priority-based inclusion.
   The per-tool cap stays.
8. **Transcript compression.** The only sanctioned mutator of stored history.
   Depends on 7.
9. **`anthropic_messages` transport.** A third `ApiMode`. It stays inside
   `llm/`.
10. **Named sessions.** `--session <id>`, plus listing and searching. The row
    exists; cwd is the only lookup today.
11. **Evaluation harness.** End-to-end scenarios with a scored outcome, then
    pass@k, latency and token usage.
12. **Learned allowlists.** Persisted consent, with a way to review and revoke
    it. "Allow Always" for one run stays as it is.
13. **Guardian review.** Hand a sensitive operation to a person out of band
    rather than blocking the loop on stdin.
14. **Task decomposition.** Break a goal into a tool sequence before acting.
15. **Sub-agents, and a TUI.** The TUI is in progress on `TUI-UX`. It lives in
    `src/tui/` and is reached with `--tui`. The one-shot CLI stays the default.
    The screen is a state machine (composer, footer, transcript, approval)
    drawn by ratatui; the runtime loop is unchanged and reports progress
    through `TurnSink`. Model-token streaming is still not in the client, so
    the live cell is the in-flight tool, not a partial completion. Sub-agents
    are still not started.

## Environment note

`.cargo/config.toml` pins `SDKROOT` to the 26.5 SDK: the default macOS 27.0 SDK
on this machine ships `.tbd` stubs the installed linker cannot parse. Remove it
if Command Line Tools are updated.
