use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::conversation::{Conversation, Message};
use crate::llm::{LlmClient, LlmError};
use crate::sandbox::{Backend, SandboxPolicy};
use crate::secrets;
use crate::session::{Session, SessionError, SessionStore};
use crate::tools::{Approval, Registry};

/// Stands in for a tool result with nothing in it. An empty string is not a
/// valid tool message on at least one provider, and "nothing" is information
/// the model needs either way.
const EMPTY_RESULT: &str = "(the tool produced no output)";

/// Most assistant turns one run may take before giving up.
const MAX_TURNS: usize = 10;

/// How many times the model may be corrected for faking a tool call before
/// the run is abandoned.
const MAX_CORRECTIONS: usize = 2;

/// Sent when the model writes tool markup into its message instead of making
/// a real tool call.
const CORRECTION: &str = "That was not a tool call. Do not write tool markup in your \
message text: it is never executed. Either call a tool through the tool-calling \
mechanism, or answer in plain text. The tools that exist are: ";

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("task is empty")]
    EmptyTask,
    #[error("model returned an empty message (finish_reason: {0})")]
    EmptyAnswer(String),
    #[error("reached the {MAX_TURNS} turn limit without a final answer")]
    TurnLimit,
    #[error("model kept writing fake tool markup instead of calling a tool")]
    FakeToolCalls,
    #[error("interrupted")]
    Cancelled,
    #[error("no session to resume in {0}; run a task here first")]
    NoSessionToResume(String),
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// The agent runtime: takes a task, returns a response.
///
/// Owns the conversation and drives the loop:
/// user -> assistant tool_call -> tool result -> assistant ... -> final answer.
pub struct AgentRuntime {
    llm: LlmClient,
    tools: Registry,
    root: PathBuf,
    sessions: Option<SessionStore>,
    resume: bool,
    auto_approve: bool,
    /// Set by the TUI. The one-shot CLI leaves this empty and keeps writing
    /// the trace to stderr.
    sink: Option<Box<dyn TurnSink>>,
    /// Shared with the screen so Esc can stop the loop between tools.
    cancel: Arc<AtomicBool>,
}

/// What the screen needs while a turn is in flight. The runtime does not draw.
#[derive(Debug)]
pub enum TurnEvent {
    /// A model call has started and nothing has come back yet.
    Thinking,
    /// A piece of the answer, as it arrives. The whole answer still follows
    /// as `Assistant`, so anything that only handles that stays correct.
    Delta(String),
    Assistant(String),
    ToolStart {
        name: String,
        arguments: String,
    },
    ToolDone {
        name: String,
        output: String,
        failed: bool,
    },
}

/// The approval question, owned so it can cross a thread.
#[derive(Debug)]
pub struct ApprovalRequest {
    pub tool: String,
    pub preview: String,
    pub concerns: Vec<String>,
    pub flagged: bool,
}

/// Where a turn reports progress. Present only for the TUI; the CLI does not
/// set one. Methods take `&self` because `run` is shared and the sink blocks
/// inside `choose` until the screen answers.
pub trait TurnSink: Send {
    fn emit(&self, event: TurnEvent);
    fn choose(&self, request: ApprovalRequest) -> ApprovalChoice;
}

/// What the user said at the approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChoice {
    Once,
    Always,
    No,
}

impl AgentRuntime {
    pub fn new(llm: LlmClient, root: PathBuf, max_tool_output: usize) -> Self {
        Self {
            llm,
            tools: Registry::new(max_tool_output),
            root,
            sessions: None,
            resume: false,
            auto_approve: false,
            sink: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Draw progress on a screen instead of stderr. The one-shot CLI never
    /// calls this.
    pub fn with_sink(mut self, sink: Box<dyn TurnSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Handle the screen holds so it can interrupt a turn it does not own.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// The next `run` continues this directory's session. The TUI sets this
    /// after the first turn so the model sees the chat already on screen.
    pub fn resume_next(&mut self) {
        if self.sessions.is_some() {
            self.resume = true;
        }
    }

    /// Offer the command tool. Without this it is absent from the tool list
    /// the model is given, not merely refused when used.
    pub fn with_exec(
        mut self,
        timeout: Duration,
        backend: Backend,
        sandbox: SandboxPolicy,
    ) -> Self {
        self.tools = self.tools.with_exec(timeout, backend, sandbox);
        self
    }

    /// Stop asking before each file change. The default is to ask.
    pub fn with_auto_approve(mut self, auto_approve: bool) -> Self {
        self.auto_approve = auto_approve;
        self
    }

    /// Record this run, and continue the directory's last conversation when
    /// `resume` is set. Without this the runtime keeps no history at all.
    pub fn with_sessions(mut self, sessions: SessionStore, resume: bool) -> Self {
        self.sessions = Some(sessions);
        self.resume = resume;
        self
    }

    pub fn run(&self, task: &str) -> Result<String, RuntimeError> {
        let task = task.trim();
        if task.is_empty() {
            return Err(RuntimeError::EmptyTask);
        }

        let specs = self.tools.specs();
        let (mut conversation, session) = self.open()?;

        // A resumed transcript already carries the system prompt it was built
        // with, restored verbatim. Rebuilding it here would change the prompt
        // under a conversation that had already been held with the old one.
        if conversation.is_empty() {
            let prompt = self.system_prompt();
            self.record(&mut conversation, &session, Message::System(prompt))?;
        }
        self.record(&mut conversation, &session, Message::User(task.to_string()))?;

        // Set once the user picks "Allow Always". Scoped to this run, so the
        // consent dies with the process and is never written down.
        let mut allow_all = false;

        // A cancel from the previous turn must not kill this one.
        self.cancel.store(false, Ordering::Relaxed);

        let mut corrections = 0;
        for _ in 0..MAX_TURNS {
            if self.cancelled() {
                return Err(RuntimeError::Cancelled);
            }
            self.note_thinking();
            // Streaming only earns its keep where someone is watching. The
            // CLI prints one answer at the end, so it takes the simpler path.
            let completion = match self.sink.is_some() {
                true => {
                    let mut on_delta = |text: &str| self.emit(TurnEvent::Delta(text.to_string()));
                    self.llm
                        .send_streaming(conversation.messages(), &specs, &mut on_delta)?
                }
                false => self.llm.send(conversation.messages(), &specs)?,
            };
            self.record(&mut conversation, &session, completion.message.clone())?;

            let Message::Assistant {
                content,
                tool_calls,
            } = completion.message
            else {
                return Err(RuntimeError::EmptyAnswer(completion.finish_reason));
            };

            if let Some(text) = content.as_ref().filter(|text| !text.trim().is_empty()) {
                self.emit(TurnEvent::Assistant(text.clone()));
            }

            if tool_calls.is_empty() {
                let text = content.unwrap_or_default();

                // A registry cannot catch this: the model wrote tool markup as
                // prose, so no tool call was ever made. Reject it and say why.
                if looks_like_fake_tool_call(&text) {
                    corrections += 1;
                    if corrections > MAX_CORRECTIONS {
                        return Err(RuntimeError::FakeToolCalls);
                    }
                    self.trace("✗ ignored invented tool call in message text");
                    conversation.push(Message::User(format!(
                        "{CORRECTION}{}.",
                        self.tools.names()
                    )));
                    continue;
                }

                return match text.trim().is_empty() {
                    false => Ok(text),
                    true => Err(RuntimeError::EmptyAnswer(completion.finish_reason)),
                };
            }

            // Run every requested tool and feed each result back. A failure is
            // reported to the model, not to the user: it can correct itself.
            for call in tool_calls {
                if self.cancelled() {
                    return Err(RuntimeError::Cancelled);
                }
                self.emit(TurnEvent::ToolStart {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                self.trace(&format!("→ {} {}", call.name, call.arguments));
                let mut approve = |request: &Approval| self.approve(request, &mut allow_all);
                let result =
                    match self
                        .tools
                        .dispatch(&call.name, &call.arguments, &self.root, &mut approve)
                    {
                        Ok(output) => output,
                        Err(err) => {
                            self.trace(&format!("  ✗ {err}"));
                            format!("error: {err}")
                        }
                    };
                let failed = result.starts_with("error:");
                // Before the model sees it, and so before it is sent to the
                // provider on every later turn. `policy.rs` refuses the files
                // it can name; this catches a credential sitting inside one
                // it cannot, such as a config file or a log.
                let result = secrets::redact(&result);
                if result.count > 0 {
                    self.trace(&format!(
                        "  ✗ redacted {} secret(s) from the result",
                        result.count
                    ));
                }

                // A tool may legitimately produce nothing: an empty file, a
                // command that printed nothing. The wire will not carry it -
                // Sarvam rejects an empty tool message with a 400 - so say so
                // in words instead of sending the emptiness.
                let content = tool_content(result.text);
                self.emit(TurnEvent::ToolDone {
                    name: call.name,
                    output: content.clone(),
                    failed,
                });

                self.record(
                    &mut conversation,
                    &session,
                    Message::Tool {
                        tool_call_id: call.id,
                        content,
                    },
                )?;
            }
        }

        Err(RuntimeError::TurnLimit)
    }

    /// Ask before a tool changes the filesystem.
    ///
    /// Silence is a no. An unanswered prompt, a closed stdin or a read error
    /// all decline, because the failure that costs the user something is
    /// writing a file they never agreed to, not refusing one they wanted.
    ///
    /// A request carrying concerns is always asked, `--yes` or not, so the
    /// flag cannot blanket-approve the commands most worth reading.
    fn approve(&self, request: &Approval, allow_all: &mut bool) -> bool {
        if !needs_asking(request, self.auto_approve, *allow_all) {
            self.trace(&format!("● {} {}", request.tool, request.preview));
            self.trace(&format!(
                "  approved by {}",
                match self.auto_approve {
                    true => "--yes",
                    false => "Allow Always",
                }
            ));
            return true;
        }

        let flagged = !request.concerns.is_empty();
        if let Some(sink) = &self.sink {
            let choice = sink.choose(ApprovalRequest {
                tool: request.tool.to_string(),
                preview: request.preview.to_string(),
                concerns: request.concerns.to_vec(),
                flagged,
            });
            return self.apply_choice(choice, flagged, allow_all);
        }

        eprintln!("\n● {} {}", request.tool, request.preview);
        for concern in request.concerns {
            eprintln!("  ! {concern}");
        }

        // A flagged call is asked every time, so "Allow Always" cannot be
        // honoured here. The option keeps its number rather than vanishing:
        // a menu that changes shape between prompts is one people misread.
        eprintln!();
        eprintln!("  1) Allow Once");
        match flagged {
            true => {
                eprintln!("  2) Allow Always - not available here, this must be answered each time")
            }
            false => {
                eprintln!("  2) Allow Always - allows every change for the rest of this session")
            }
        }
        eprintln!("  3) No");
        eprint!("  choose [1/2/3]: ");
        let _ = std::io::stderr().flush();

        let mut answer = String::new();
        let decision = match std::io::stdin().read_line(&mut answer) {
            Ok(0) | Err(_) => {
                if !std::io::stdin().is_terminal() {
                    eprintln!("(no answer; declined)");
                }
                ApprovalChoice::No
            }
            Ok(_) => decide(&answer),
        };

        let allowed = self.apply_choice(decision, flagged, allow_all);
        match decision {
            ApprovalChoice::Always if !flagged && allowed => {
                eprintln!("  ✓ every change approved for the rest of this session");
            }
            ApprovalChoice::No => eprintln!("  ✗ declined"),
            _ => {}
        }
        allowed
    }

    fn apply_choice(&self, choice: ApprovalChoice, flagged: bool, allow_all: &mut bool) -> bool {
        match choice {
            ApprovalChoice::Always if !flagged => {
                *allow_all = true;
                true
            }
            ApprovalChoice::Always | ApprovalChoice::Once => true,
            ApprovalChoice::No => false,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn emit(&self, event: TurnEvent) {
        if let Some(sink) = &self.sink {
            sink.emit(event);
        }
    }

    fn note_thinking(&self) {
        self.emit(TurnEvent::Thinking);
    }

    /// Stderr when nobody is drawing. A sink already has the structured events,
    /// and a line on stderr would land on top of the screen.
    fn trace(&self, line: &str) {
        if self.sink.is_none() {
            eprintln!("{line}");
        }
    }

    /// Restore this directory's conversation, or begin one. Without a store
    /// the run is one-shot, exactly as it was before sessions existed.
    fn open(&self) -> Result<(Conversation, Option<Session<'_>>), RuntimeError> {
        let Some(store) = &self.sessions else {
            return Ok((Conversation::new(), None));
        };
        let root = self.root.to_string_lossy();

        if self.resume {
            let resumed = store
                .resume(&root)?
                .ok_or_else(|| RuntimeError::NoSessionToResume(root.to_string()))?;
            return Ok((
                Conversation::from_messages(resumed.messages),
                Some(resumed.session),
            ));
        }
        Ok((
            Conversation::new(),
            Some(store.start(&root, self.llm.model())?),
        ))
    }

    /// Disk first, then memory. If the write fails the run stops instead of
    /// carrying on against a transcript that will not come back.
    fn record(
        &self,
        conversation: &mut Conversation,
        session: &Option<Session<'_>>,
        message: Message,
    ) -> Result<(), RuntimeError> {
        if let Some(session) = session {
            session.append(conversation.len(), &message)?;
        }
        conversation.push(message);
        Ok(())
    }

    /// The tool names come from the registry, so the prompt cannot advertise a
    /// tool that does not exist.
    fn system_prompt(&self) -> String {
        format!(
            "You are token, a coding agent that works inside the user's project directory. \
The only tools that exist are: {}. Call them through the tool-calling mechanism; never \
write tool markup such as <tool_call> in your message text, and never assume a tool you \
have not been given. Use read_file to look at files instead of guessing or asking the \
user to paste them. Paths are relative to the project root, for example src/main.rs. \
Every write is shown to the user, who may refuse it; if they do, do not try to make the \
same change another way. Files holding secrets cannot be read or written, .env among \
them: to document a variable, write .env.example with a placeholder and never a real \
value. When you have what you need, answer directly.",
            self.tools.names()
        )
    }
}

/// A tool result as it goes on the wire. Nothing becomes words: an empty
/// string is not a valid tool message on at least one provider, and "the tool
/// produced nothing" is information the model needs either way.
fn tool_content(text: String) -> String {
    match text.is_empty() {
        true => EMPTY_RESULT.to_string(),
        false => text,
    }
}

/// Read the answer. The menu is numbered, but the words it prints are
/// accepted too, because someone reading "Allow Once" will type it.
///
/// Anything unrecognised is a no: the costly mistake is acting on consent the
/// user did not give, so a typo declines rather than guessing.
fn decide(answer: &str) -> ApprovalChoice {
    match answer.trim().to_ascii_lowercase().as_str() {
        "1" | "once" | "allow once" | "y" | "yes" => ApprovalChoice::Once,
        "2" | "always" | "allow always" | "a" => ApprovalChoice::Always,
        _ => ApprovalChoice::No,
    }
}

/// Whether this request has to go to the user.
///
/// A concern always does, whatever standing consent exists: `--yes` and
/// "Allow Always" both cover the routine case, and a call naming a secret or
/// leaving the project is the one the user meant to see.
fn needs_asking(request: &Approval, auto_approve: bool, allow_all: bool) -> bool {
    if !request.concerns.is_empty() {
        return true;
    }
    !(auto_approve || allow_all)
}

/// Markup a model emits when it imagines a tool it was never given. Matched
/// against real output seen from this model, not invented patterns.
fn looks_like_fake_tool_call(text: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "<tool_call>",
        "<function_call>",
        "<arg_key>",
        "<invoke name=",
        "<invoke",
    ];
    let text = text.to_ascii_lowercase();
    MARKERS.iter().any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(tool: &'a str, concerns: &'a [String]) -> Approval<'a> {
        Approval {
            tool,
            preview: "preview",
            concerns,
        }
    }

    #[test]
    fn an_empty_tool_result_is_replaced_with_words() {
        // Observed live: read_file on an empty file produced an empty tool
        // message and Sarvam answered 400 "String should have at least 1
        // character", ending the run.
        assert_eq!(tool_content(String::new()), EMPTY_RESULT);
        assert!(EMPTY_RESULT.contains("no output"));
    }

    #[test]
    fn a_tool_result_with_content_is_passed_through() {
        assert_eq!(tool_content("fn main() {}".into()), "fn main() {}");
        // Whitespace is output: a command that printed a blank line said
        // something, and trimming it away would be a different answer.
        assert_eq!(tool_content("\n".into()), "\n");
    }

    #[test]
    fn reads_the_numbered_answers() {
        assert_eq!(decide("1"), ApprovalChoice::Once);
        assert_eq!(decide("2"), ApprovalChoice::Always);
        assert_eq!(decide("3"), ApprovalChoice::No);
    }

    #[test]
    fn reads_the_words_the_menu_prints() {
        // Someone shown "Allow Once" will type it rather than its number.
        assert_eq!(decide("once"), ApprovalChoice::Once);
        assert_eq!(decide("Allow Once"), ApprovalChoice::Once);
        assert_eq!(decide("always"), ApprovalChoice::Always);
        assert_eq!(decide(" ALLOW ALWAYS \n"), ApprovalChoice::Always);
        assert_eq!(decide("no"), ApprovalChoice::No);
    }

    #[test]
    fn anything_unrecognised_declines() {
        // A typo must not be read as consent.
        assert_eq!(decide(""), ApprovalChoice::No);
        assert_eq!(decide("\n"), ApprovalChoice::No);
        assert_eq!(decide("4"), ApprovalChoice::No);
        assert_eq!(decide("yolo"), ApprovalChoice::No);
        assert_eq!(decide("allow"), ApprovalChoice::No);
    }

    #[test]
    fn a_fresh_run_asks_about_everything() {
        assert!(needs_asking(&request("terminal", &[]), false, false));
        assert!(needs_asking(&request("write_file", &[]), false, false));
    }

    #[test]
    fn allow_always_covers_every_tool_for_the_rest_of_the_run() {
        assert!(!needs_asking(&request("terminal", &[]), false, true));
        assert!(!needs_asking(&request("write_file", &[]), false, true));
    }

    #[test]
    fn a_concern_is_asked_despite_standing_consent() {
        // The whole point of the escalation: neither --yes nor Allow Always
        // covers a call naming a secret or leaving the project.
        let flagged = ["`.env` is off limits".to_string()];
        assert!(needs_asking(&request("terminal", &flagged), false, true));
        assert!(needs_asking(&request("terminal", &flagged), true, true));
    }

    #[test]
    fn yes_covers_the_routine_case() {
        assert!(!needs_asking(&request("terminal", &[]), true, false));
    }

    #[test]
    fn detects_the_markup_this_model_actually_emitted() {
        let seen = "<tool_call>terminal\n<arg_key>command</arg_key>\n\
<arg_value>file target/debug/token</arg_value>\n</tool_call>";
        assert!(looks_like_fake_tool_call(seen));
    }

    #[test]
    fn detects_markup_in_other_casings_and_formats() {
        assert!(looks_like_fake_tool_call(
            "sure!\n<FUNCTION_CALL>bash</FUNCTION_CALL>"
        ));
        assert!(looks_like_fake_tool_call("<invoke name=\"terminal\">"));
    }

    #[test]
    fn leaves_ordinary_answers_alone() {
        assert!(!looks_like_fake_tool_call(
            "The runtime calls tools via the tool_calls field; see src/runtime.rs."
        ));
    }

    #[test]
    fn leaves_code_about_tool_calls_alone() {
        // Explaining our own code must not be mistaken for faking a call.
        assert!(!looks_like_fake_tool_call(
            "```rust\nfor call in tool_calls {\n    self.tools.dispatch(&call.name)\n}\n```"
        ));
    }
}
