# Plan

One CLI. One agent loop. The person picks a workspace, and the same runtime
drives it: observe, call a tool, observe again, stop with an answer. Coding is
the workspace that exists. Financial market research, go-to-market, and lead
generation are further workspaces on that loop. They do not get their own
binary, their own conversation type, or a path around approval.

A workspace is three things: a system prompt, the tools it is allowed to see,
and the files it is allowed to write. The model still only receives a
`ToolSpec` from a `Tool` impl. MCP servers, built-in or added by the user, are
adapters into that same registry.

## What is already built

This is the coding workspace, on the current tree.

| Piece | Where | What it does |
|---|---|---|
| Composition | `main.rs` | Wires the provider, sandbox, sessions, and either one task or `--tui`. |
| Args | `cli.rs` | Provider, model, base URL, API mode, tool-output cap, exec timeout, sandbox, network, `--yes`, `--resume`, `--no-session`, `--tui`. |
| Conversation | `conversation.rs` | Provider-neutral `Message`, `ToolCall`, `Conversation`. |
| Loop | `runtime.rs` | Write-through turns, fake tool-markup correction, cancel between tools, `TurnSink` for the screen. `MAX_TURNS` is 10. |
| Sessions | `session.rs` | SQLite at `~/.token/sessions.db`, keyed by directory. `--resume` reloads the latest and repairs a transcript cut off mid-tool-call. |
| Models | `llm/` | OpenAI and Sarvam. Chat completions and the Responses API. The host picks the wire format, not the model name. Streaming deltas reach the screen as `TurnEvent::Delta`. |
| Tools | `tools.rs` | `read_file`, `write_file`, `terminal`. Spec and handler are the same object. Failures go back to the model. |
| Approval | `tools.rs`, `runtime.rs` | Numbered menu: Allow Once, Allow Always, No. Silence is a no. Allow Always dies with the process. A command that names a secret or leaves the project is always asked, including under `--yes`. |
| Policy | `policy.rs` | Refuses secrets and paths outside the project before a file is opened. Screens command text for the prompt. That screen is not confinement. |
| Secrets | `secrets.rs` | Redacts tool results before the model sees them, and messages before SQLite. |
| Sandbox | `sandbox.rs` | macOS Seatbelt. `workspace-write` is the default. Remote network is denied unless `--allow-network`. Loopback stays open. Elsewhere the command runs unconfined and the program says so. |
| Screen | `src/tui/` | `--tui`. Transcript, live tool cell, streaming deltas, composer, footer, approval popup. One-shot mode remains the default. |

Still open on the coding side, and still worth finishing before a workspace
depends on them:

- `list_files`, `grep`, and `apply_patch` (a write today replaces the whole file).
- A turn-limit flag, and a stop message that says what already happened.
- `max_tokens` / `reasoning_effort` / `max_output_tokens`. The values are still an owner decision.
- Token budgeting and, after that, transcript compression.
- Named sessions, list, and search.
- Anthropic as a third `ApiMode`, inside `llm/` only.
- An evaluation harness: a scored scenario, then pass@k, latency, and token use.
- Prompt text moved out of the `format!` in `runtime.rs` so each workspace prompt can be diffed and tested.

## The rule that does not change

New work slots under the layers that exist.

- A new capability is a `Tool` registered in `Registry`, or an MCP tool adapted into one. No hand-written spec beside a separate handler.
- Arguments stay strict.
- A tool failure is a result the model can correct. Only a broken runtime reaches the user.
- Anything that changes the machine, sends a message, or moves money sets `mutates` and goes through the approval menu.
- Secrets are refused before they are read, and redacted if they appear anyway.
- The sandbox is the only real boundary for shell commands. String matching on a command is a warning, and the plan must not describe it as containment.
- The model must not be told a fact, a number, a client, a quote, or a price that is not in a tool result, a file, or a source the user supplied. Missing data is written as `[CONFIRM]` or `[NUMBER NEEDED]`, not filled in.
- Nothing is posted, sent, scheduled, or traded without an explicit approval on that action.

## Workspaces

`--workspace coding` is the default and today's behaviour. The others are:

| Workspace | Job | Writes |
|---|---|---|
| `coding` | Change and explain this repository. | The project tree, after approval. |
| `markets` | Research a name, a sector, a filing, or a move. Produce a sourced note. | `workspaces/markets/` notes only. |
| `gtm` | Positioning, offer, channel plan, and draft assets for a product. | `workspaces/gtm/` drafts only. |
| `leads` | Find accounts, score them, and draft outreach. | `workspaces/leads/` records and drafts only. |

A workspace does not see another workspace's mutating tools. `markets` does
not get `write_file` on the source tree. `coding` does not get a send-mail
tool. Shared read tools (files in the workspace folder, web search, fetch) are
available in all of them.

The system prompt is chosen with the workspace and stored with the session.
Resuming a session keeps the prompt it was started with, which is already the
rule for coding.

Each workspace gets a small on-disk layout the agent can read and, after
approval, write:

```
workspaces/
  markets/   notes, watchlists, source logs
  gtm/       icp, offer, voice, calendar, drafts
  leads/     accounts, briefs, pipeline, outbox
```

Those folders are the project for that workspace. The existing write
containment applies, with that folder as the root instead of the git root.
Coding keeps the git root.

## MCP

MCP is how a workspace reaches a system this binary does not implement.
The CLI speaks the client side. It does not become a general MCP server.

Two sources, one registry:

1. **Built-in servers**, shipped and started by the CLI when the workspace
   needs them. The user does not install these to get the default experience.
2. **User servers**, declared in `~/.token/mcp.json` or a project
   `token.mcp.json`. A user server is opt-in per workspace. A config entry
   names the command or URL, the env it may see, and which workspaces may
   call it.

Each advertised MCP tool is wrapped in a `Tool` impl at startup. The spec the
model sees is the spec that runs. If the server cannot be started, that tool
is absent from the list. A missing tool is better than a tool that fails on
every call, because the model invents workarounds for tools it can see and
cannot use.

Transport, in order:

- stdio first. The child is started with `policy::scrub_env`, same block list
  as `terminal` (no KEY, SECRET, TOKEN, PASSWORD, AUTH, CREDENTIAL, PRIVATE
  unless the config names that variable on purpose).
- Streamable HTTP later, for hosted servers. The URL and the token live in
  the config, not in the prompt.

Policy on every MCP call:

- The call is classified read or mutate from the server's own annotations when
  they exist, and as mutate when they do not. Unknown means ask.
- A mutating call uses the same numbered menu as `write_file`.
- Output passes through redaction and the tool-output cap before it enters
  the conversation.
- A server that needs the network is denied unless `--allow-network` is set,
  or the config grants that one server. The grant is printed at startup so
  the user can see which process is allowed to leave the machine.

Built-in servers to ship, not optional plugins:

| Server | Used by | What it is for |
|---|---|---|
| `web_search` | all | Search the public web and return titles, URLs, and snippets. |
| `fetch` | all | GET one URL and return readable text. No login, no POST. |
| `filings` | markets | Company filings and the primary document URL (EDGAR-style public sources). |
| `quotes` | markets | Delayed price, range, and volume for a symbol. Stamped with the as-of time. |
| `news` | markets, gtm | Recent headlines for a name or a topic, each with a URL. |
| `crm_readonly` | leads | Read accounts and stages from a user-supplied CRM MCP if configured. The built-in is the local `pipeline` file when no CRM is connected. |

User-supplied MCPs cover the rest: a broker, a mail sender, LinkedIn, a data
warehouse, Notion, a calendar. Those stay out of the binary because each one
is an account and a credential. The CLI's job is to run them under the same
approval and redaction rules, and to refuse to start one whose config asks
for a secret env var the scrubber would have removed, unless that var is
listed by name in the config.

## Web search

Search is a tool, not a hidden step inside the model.

`web_search` takes a query and a small result cap. `fetch` takes one URL from
those results, or a URL the user typed. The model cites the URL it actually
fetched. A claim with no source in the tool results is not written as fact.

Search is on for every workspace once `--allow-network` is set, because the
result leaves the process only as text in the transcript. The sandbox grant
is the same flag the shell already uses. Without it, the tool is not
registered, and the prompt says web search is off.

## Financial market research

The workspace answers questions like "what changed for this company this
week" or "compare these two names on the last four quarters." It produces a
note. It does not place orders.

A note has a fixed shape, so a run can be checked:

- Question.
- As-of time.
- Findings, each with a source URL or a filing id from a tool result.
- Numbers only when a tool returned them. Otherwise `[NUMBER NEEDED]`.
- What would change the conclusion.
- What the note does not cover.

Tools the workspace sees:

- `web_search`, `fetch`, `filings`, `quotes`, `news`.
- Read and write inside `workspaces/markets/`.
- No `terminal`, unless the user passes a flag that turns it on for that run.
  A market note does not need a shell, and a shell is the widest tool we have.

Hard limits, enforced in policy rather than in the prompt:

- No order, transfer, or payment tool exists in this workspace.
- A user MCP annotated as trading is not registered here even if it is in the
  config. It can be registered only in a future workspace that does not exist
  yet, and only after a separate decision.
- Quotes are labelled delayed unless the tool result says otherwise.
- The agent does not compute a target price and present it as research. A
  target price is the user's number or `[NUMBER NEEDED]`.

## Marketing and go-to-market

The workspace turns a product description into a plan and into drafts. It
does not publish them.

Files it keeps, and will not invent if they are empty:

- `icp.md` — who buys, the words they use, who is wrong.
- `offer.md` — what is sold, the result, the price if the user wrote one, objections.
- `voice.md` — how the user writes, with quotes taken from their own past copy.
- `drafts/` — posts, pages, one-pagers, sequences. Each draft is a file, not a send.

A GTM answer covers, and skips a section it cannot source:

- Who the offer is for, copied from `icp.md` or marked `[CONFIRM]`.
- The promise, copied from `offer.md`.
- Channels, with why each one matches the ICP.
- A 2-week calendar of drafts, written into `drafts/`, not posted.
- Objections and the reply, only using objections already in `offer.md` or supplied in the task.

Tools: `web_search` and `fetch` for public competitor pages and docs; read
and write inside `workspaces/gtm/`. No social, email, or ads API unless the
user adds that MCP, and a call that publishes is always an approval, never
`--yes`.

Voice is a constraint. Drafts do not use a stock cadence, and they do not
claim a customer, a metric, or a logo that is not in the workspace files.

## Lead generation

The workspace builds a list and a next action. It does not contact anyone
until the user approves that specific draft.

Records:

- `accounts.md` — one account per entry: name, why it is on the list, the signal, the source URL, a stage.
- `briefs/` — a short brief before any outreach is written.
- `pipeline.md` — stage of each conversation the user has actually had.
- `outbox/` — drafts waiting for approval. The status on each is `WAITING`, then `APPROVED`, `EDITED`, or `REJECTED`. Nothing is deleted.

Stages are only these: new, briefed, drafted, approved, sent, replied, won,
lost. `sent` is set by the user, or by a send tool after they approved that
send. The model does not move an account to `sent` on its own.

A lead is hot, and the run stops and asks the user, when the other side asks
about price, a call, a proposal, or a timeline. The agent does not pitch a
hot lead.

Tools: `web_search`, `fetch`, read and write inside `workspaces/leads/`, and
`crm_readonly` when a CRM MCP is configured. A send tool from a user MCP is
visible only here, and every call is an approval even when Allow Always is on
for file edits. Sending is the action the user meant to see.

Scoring is a written reason, not a fake precision. "Score 87" is not allowed
unless a tool returned that score. The brief says why the account is worth a
message this week, in a sentence, with the source.

## How a run is chosen

```
token --workspace markets "What did the last 10-Q change for <company>?"
token --workspace gtm "Draft this week's page from offer.md"
token --workspace leads "Who on the list has a fresh signal?"
token --tui
```

`--tui` without a workspace opens coding, which is the default. The footer
shows the workspace next to the model and the sandbox mode. Switching
workspace starts a new session. It does not continue a coding transcript
under a markets prompt.

## Order

One item at a time, finished and tested before the next. Coding gaps that
other workspaces need come first.

1. **Workspace switch.** A prompt and a root directory per workspace. Coding stays the default and keeps today's tests.
2. **Prompt files.** Move the coding prompt out of `runtime.rs`. Add the three other prompts as files with tests that they name only tools the workspace actually registers.
3. **MCP client, stdio.** Start a server, list its tools, register adapters, scrub the child env, fail closed when the server is down.
4. **Built-in `web_search` and `fetch`.** Off unless `--allow-network`. Citations in the tool result. Tests on fixtures, not on the live web.
5. **`markets` note shape.** `filings`, `quotes`, and `news` behind the same adapter. A fixture note that refuses an unsourced number.
6. **`gtm` files.** Read ICP, offer, and voice. Write drafts only under `workspaces/gtm/drafts/`. A test that a missing price stays `[NUMBER NEEDED]`.
7. **`leads` records.** Accounts, briefs, pipeline, outbox statuses. Hot-lead stop. Send stays an approval even under `--yes`.
8. **User `mcp.json`.** Enable a server per workspace. Print the network grant at startup.
9. **Evaluation.** One scored scenario per workspace: a coding edit, a markets note, a GTM draft, a lead brief. The grader checks structure and citations, not prose quality.

## Decisions still open

These stay with the owner before the code picks them:

- Which search provider the built-in `web_search` calls, and the daily cap.
- Which public source backs `quotes` and `filings`, and whether quotes are delayed only.
- Whether `terminal` is ever offered outside `coding`.
- The `mcp.json` schema and whether a project file overrides the home file or the reverse.
- Whether a future trading workspace is in scope at all. This plan leaves it out.
- Turn limit, request token limits, and how a long research note is compressed in the session.

## Out of scope until a later plan

Sub-agents. A learned allowlist that survives the process. An async reviewer
who is not at the keyboard. Posting, sending, or trading as a default path.
A Linux or Windows sandbox. A TUI redesign per workspace: the same screen
draws every workspace, and the footer names which one is active.
