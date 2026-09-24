//! Tools the model can call.
//!
//! The model is never trusted to name its own tools. Every tool is a handler
//! that carries both its schema and its implementation, the registry builds
//! the model-visible list from those same handlers, and dispatch is an exact
//! lookup: an invented name cannot reach any code.

use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::policy;
use crate::sandbox::{self, Backend, SandboxPolicy};

/// Tier 1, the filesystem limit: the largest file we will read into memory at
/// all. This only stops absurd reads; anything under it is read and then
/// truncated to the context cap below.
const MAX_READ_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// The largest file the agent may write. Content comes from the model, so
/// this is a backstop against a runaway generation, not a workflow limit.
const MAX_WRITE_FILE_BYTES: usize = 8 * 1024 * 1024;

/// How long a command may run before its process group is killed. A hung
/// build must not hang the agent.
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 120;

/// How often the runtime checks whether a command has finished.
const EXEC_POLL: Duration = Duration::from_millis(20);

/// Lines of new content shown to the human when asking to approve a write.
const PREVIEW_LINES: usize = 12;

/// Tier 2, the context cap: how much of any tool's output reaches the model.
/// The default suits a 128K-token model; larger models can raise it, which is
/// why it is a parameter rather than a constant.
pub const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// Errors a tool reports back to the model. These are not fatal: the runtime
/// turns them into a failed tool result so the model can correct itself.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unsupported call: `{name}`. Available tools: {available}")]
    UnsupportedTool { name: String, available: String },
    #[error("failed to parse function arguments: {0}")]
    InvalidArguments(serde_json::Error),
    #[error("`{0}` is a directory, not a file")]
    IsDirectory(String),
    #[error("`{0}` is not a regular file")]
    NotRegularFile(String),
    #[error("`{path}` is not valid UTF-8 text")]
    NotText { path: String },
    #[error("could not read `{path}`: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("file is {size} bytes, larger than the {MAX_READ_FILE_BYTES} byte read limit")]
    TooLarge { size: u64 },
    #[error("`{path}` is off limits: {reason}")]
    Private { path: String, reason: &'static str },
    #[error("the user declined the change to `{path}`")]
    Denied { path: String },
    #[error("`{path}` is outside the project directory; writes stay inside it")]
    OutsideRoot { path: String },
    #[error("`{path}` is a symlink; write to the file it points at instead")]
    IsSymlink { path: String },
    #[error("content is {size} bytes, larger than the {MAX_WRITE_FILE_BYTES} byte write limit")]
    WriteTooLarge { size: usize },
    #[error(
        "`{key}` looks like a real secret and `{path}` is a shared file that gets committed. \
Write a placeholder such as `{key}=your_value_here`, and let the user put the real value in \
their own .env"
    )]
    SecretInExample { path: String, key: String },
    #[error("could not create `{path}`: {source}")]
    CreateDir {
        path: String,
        source: std::io::Error,
    },
    #[error("could not run the command: {0}")]
    Spawn(std::io::Error),
    #[error("command was killed after {secs}s without finishing")]
    ExecTimeout { secs: u64 },
}

/// A tool as the model should see it, in no provider's shape. Each transport
/// in `llm` renders this into the JSON its own endpoint expects: chat
/// completions nests it under `function`, the Responses API keeps it flat.
/// This module does not know which, and must not.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// What the human is being asked to allow. A struct rather than three
/// arguments because `concerns` is the part that changes what the answer is
/// allowed to be: a request carrying any cannot be waved through by `--yes`.
pub struct Approval<'a> {
    pub tool: &'a str,
    pub preview: &'a str,
    pub concerns: &'a [String],
}

/// A tool the model can call. One implementation carries both the schema the
/// model sees and the code that runs, so the two cannot drift apart.
pub trait Tool {
    fn name(&self) -> &'static str;
    fn spec(&self) -> ToolSpec;
    fn call(&self, arguments: &str, root: &Path) -> Result<String, ToolError>;

    /// Whether a call changes anything on disk. The registry asks the human
    /// before dispatching one of these, and never for anything else.
    fn mutates(&self) -> bool {
        false
    }

    /// What the human is being asked to approve. Pure: it may read the
    /// filesystem to describe the change, but must not make it.
    fn preview(&self, arguments: &str, _root: &Path) -> Result<String, ToolError> {
        Ok(arguments.to_string())
    }

    /// Reasons this particular call deserves a fresh answer even when the
    /// user has said yes to everything. Empty for a tool whose arguments are
    /// already confined by the time they get here.
    fn concerns(&self, _arguments: &str, _root: &Path) -> Vec<String> {
        Vec::new()
    }
}

/// Every tool that exists. The model-visible list and dispatch both read from
/// here, so anything the model can see is callable and anything else is not.
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
    max_output_bytes: usize,
}

impl Registry {
    pub fn new(max_output_bytes: usize) -> Self {
        Self {
            tools: vec![Box::new(ReadFile), Box::new(WriteFile)],
            max_output_bytes,
        }
    }

    /// Add the command tool. Off by default, and when it is off the tool is
    /// absent from the spec entirely rather than refused on use: a tool the
    /// model cannot see is one it does not keep trying.
    pub fn with_exec(
        mut self,
        timeout: Duration,
        backend: Backend,
        sandbox: SandboxPolicy,
    ) -> Self {
        self.tools.push(Box::new(Terminal {
            timeout,
            backend,
            sandbox,
        }));
        self
    }

    /// The tool list sent to the model, generated from the handlers above.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    /// Tool names, for the system prompt and for error messages.
    pub fn names(&self) -> String {
        self.tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Run a tool by the exact name the model used. An unknown name is
    /// rejected here and never reaches a handler.
    ///
    /// `approve` is asked before any tool that changes the filesystem, and is
    /// never asked for one that does not. It is a parameter so this module
    /// stays free of terminal I/O and so a test can answer for itself.
    pub fn dispatch(
        &self,
        name: &str,
        arguments: &str,
        root: &Path,
        approve: &mut dyn FnMut(&Approval) -> bool,
    ) -> Result<String, ToolError> {
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == name) else {
            return Err(ToolError::UnsupportedTool {
                name: name.to_string(),
                available: self.names(),
            });
        };

        if tool.mutates() {
            // Arguments are validated by `preview` first, so a malformed call
            // is refused before anyone is asked to approve it.
            let preview = tool.preview(arguments, root)?;
            let concerns = tool.concerns(arguments, root);
            let request = Approval {
                tool: tool.name(),
                preview: &preview,
                concerns: &concerns,
            };
            if !approve(&request) {
                return Err(ToolError::Denied {
                    path: preview.lines().next().unwrap_or(name).to_string(),
                });
            }
        }

        // Every tool's output passes through the context cap, not just
        // read_file: an exec tool's output would need the same guard.
        tool.call(arguments, root)
            .map(|output| truncate_for_context(output, self.max_output_bytes))
    }
}

struct ReadFile;

/// Arguments are strict: an invented field such as `offset` is rejected with a
/// message the model can act on, rather than being silently ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: String,
}

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name(),
            description: "Read a UTF-8 text file and return its contents. Reads from \
the start of the file and takes no offset or limit; output longer than the tool output \
limit is truncated, and the truncation is stated in the result.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file. Relative paths resolve against the project root, e.g. src/main.rs; absolute paths such as /etc/hosts are also allowed."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, arguments: &str, root: &Path) -> Result<String, ToolError> {
        let args: ReadFileArgs =
            serde_json::from_str(arguments).map_err(ToolError::InvalidArguments)?;
        read_file(&args.path, root)
    }
}

fn read_file(path: &str, root: &Path) -> Result<String, ToolError> {
    let resolved = resolve(path, root)?;

    // Check the metadata before opening. A directory, a device such as
    // /dev/urandom, or a fifo would otherwise either error confusingly or
    // read without end.
    let metadata = std::fs::metadata(&resolved).map_err(|source| ToolError::Io {
        path: path.to_string(),
        source,
    })?;
    if metadata.is_dir() {
        return Err(ToolError::IsDirectory(path.to_string()));
    }
    if !metadata.is_file() {
        return Err(ToolError::NotRegularFile(path.to_string()));
    }
    if metadata.len() > MAX_READ_FILE_BYTES {
        return Err(ToolError::TooLarge {
            size: metadata.len(),
        });
    }

    let bytes = std::fs::read(&resolved).map_err(|source| ToolError::Io {
        path: path.to_string(),
        source,
    })?;
    // Size is rechecked: metadata can be stale, and some files report 0.
    if bytes.len() as u64 > MAX_READ_FILE_BYTES {
        return Err(ToolError::TooLarge {
            size: bytes.len() as u64,
        });
    }

    String::from_utf8(bytes).map_err(|_| ToolError::NotText {
        path: path.to_string(),
    })
}

/// Resolve `path` to a real file. Relative paths resolve against `root`;
/// absolute paths are used as given.
///
/// The privacy check runs on the name as given and again on the resolved
/// path, so a symlink pointing at a secret is refused as well as the secret
/// itself.
fn resolve(path: &str, root: &Path) -> Result<PathBuf, ToolError> {
    let joined = root.join(path);
    refuse_if_private(path, &joined)?;

    let resolved = joined.canonicalize().map_err(|source| ToolError::Io {
        path: path.to_string(),
        source,
    })?;
    refuse_if_private(path, &resolved)?;
    Ok(resolved)
}

fn refuse_if_private(reported: &str, path: &Path) -> Result<(), ToolError> {
    match policy::private_reason(path) {
        Some(reason) => Err(ToolError::Private {
            path: reported.to_string(),
            reason,
        }),
        None => Ok(()),
    }
}

/// Resolve a path to write to. Stricter than `resolve` in three ways: the
/// target need not exist, it must stay inside the project, and a symlink is
/// refused rather than followed, because following one is how a write escapes
/// a directory that was already checked.
///
/// Containment is decided lexically, before anything is created. Deciding it
/// afterwards would mean `../elsewhere/f.txt` had already made a directory
/// outside the project by the time it was refused.
fn resolve_for_write(path: &str, root: &Path) -> Result<PathBuf, ToolError> {
    // Canonicalise the root first. On macOS the temp directory and /var are
    // symlinks, so comparing a canonical path against an uncanonical root
    // says "outside" for paths that are plainly inside.
    let root = root.canonicalize().map_err(|source| ToolError::Io {
        path: root.display().to_string(),
        source,
    })?;

    // `join` with an absolute path replaces the root, which is how an
    // absolute escape arrives here and gets caught by the check below.
    let target = normalise(&root.join(path));
    if !target.starts_with(&root) {
        return Err(ToolError::OutsideRoot {
            path: path.to_string(),
        });
    }
    refuse_if_private(path, &target)?;

    let parent = target.parent().unwrap_or(&root).to_path_buf();
    if !parent.exists() {
        std::fs::create_dir_all(&parent).map_err(|source| ToolError::CreateDir {
            path: parent.display().to_string(),
            source,
        })?;
    }

    // Re-check against the real parent: a lexical path stays inside the root
    // even when a directory along it is a symlink leading out.
    let parent = parent.canonicalize().map_err(|source| ToolError::Io {
        path: path.to_string(),
        source,
    })?;
    if !parent.starts_with(&root) {
        return Err(ToolError::OutsideRoot {
            path: path.to_string(),
        });
    }

    // `symlink_metadata` does not follow the link, which is the point.
    if let Ok(metadata) = std::fs::symlink_metadata(&target) {
        if metadata.file_type().is_symlink() {
            return Err(ToolError::IsSymlink {
                path: path.to_string(),
            });
        }
        if metadata.is_dir() {
            return Err(ToolError::IsDirectory(path.to_string()));
        }
    }
    Ok(target)
}

/// Resolve `.` and `..` without touching the filesystem, so a path can be
/// judged before any of it is created.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

struct WriteFile;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    path: String,
    content: String,
}

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn mutates(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name(),
            description: "Write a UTF-8 text file, creating it or replacing it whole. \
There is no append and no partial edit: pass the complete intended contents. The user \
is asked to approve every write and may refuse. Paths stay inside the project; files \
holding secrets, such as .env, cannot be written. .env.example can be written, but only \
with placeholder values such as your_key_here, never a real credential.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the project root, e.g. src/main.rs. It must stay inside the project."
                    },
                    "content": {
                        "type": "string",
                        "description": "The complete new contents of the file."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    /// Describes the change without making it, so the human sees what they
    /// are approving rather than a tool name.
    fn preview(&self, arguments: &str, root: &Path) -> Result<String, ToolError> {
        let args: WriteFileArgs =
            serde_json::from_str(arguments).map_err(ToolError::InvalidArguments)?;
        let target = resolve_for_write(&args.path, root)?;
        check_write(&args.path, &target, &args.content)?;

        let existing = std::fs::metadata(&target).ok().map(|meta| meta.len());
        let summary = match existing {
            Some(size) => format!(
                "{} (overwrite, {} bytes -> {} bytes)",
                args.path,
                size,
                args.content.len()
            ),
            None => format!("{} (new file, {} bytes)", args.path, args.content.len()),
        };

        let mut preview = String::from(&summary);
        for line in args.content.lines().take(PREVIEW_LINES) {
            preview.push_str("\n  | ");
            preview.push_str(line);
        }
        let total = args.content.lines().count();
        if total > PREVIEW_LINES {
            preview.push_str(&format!("\n  | ... {} more lines", total - PREVIEW_LINES));
        }
        Ok(preview)
    }

    fn call(&self, arguments: &str, root: &Path) -> Result<String, ToolError> {
        let args: WriteFileArgs =
            serde_json::from_str(arguments).map_err(ToolError::InvalidArguments)?;
        let target = resolve_for_write(&args.path, root)?;
        check_write(&args.path, &target, &args.content)?;

        let existed = target.exists();
        std::fs::write(&target, &args.content).map_err(|source| ToolError::Io {
            path: args.path.clone(),
            source,
        })?;

        Ok(format!(
            "{} {} ({} bytes)",
            if existed { "replaced" } else { "created" },
            args.path,
            args.content.len()
        ))
    }
}

/// Everything that must hold before a write, in one place so `preview` and
/// `call` cannot drift: the same refusal the user sees is the one applied.
fn check_write(path: &str, target: &Path, content: &str) -> Result<(), ToolError> {
    check_write_size(content)?;
    if policy::is_shareable_env_file(target) {
        if let Some(key) = policy::committed_secret(content) {
            return Err(ToolError::SecretInExample {
                path: path.to_string(),
                key,
            });
        }
    }
    Ok(())
}

/// Named `terminal` on purpose. Before this tool existed the model kept
/// inventing one by that name, with a `command` argument, and writing
/// `<tool_call>terminal` into its message text. Matching the name it already
/// reaches for turns those attempts into real calls instead of rejections.
struct Terminal {
    timeout: Duration,
    backend: Backend,
    sandbox: SandboxPolicy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalArgs {
    command: String,
}

impl Tool for Terminal {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn mutates(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name(),
            description: "Run a shell command in the project directory and return its \
output and exit status. The user is shown the command and may refuse it. There is no \
interactive input: a command that waits for stdin will time out. Credentials are removed \
from the environment, so printing them is not possible.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command, e.g. cargo test or ls src. Runs with the project root as the working directory."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    /// The command itself is what the human is approving. Nothing is
    /// summarised away: the whole point is that they read it.
    fn preview(&self, arguments: &str, root: &Path) -> Result<String, ToolError> {
        let args: TerminalArgs =
            serde_json::from_str(arguments).map_err(ToolError::InvalidArguments)?;
        Ok(format!("{}\n  in {}", args.command, root.display()))
    }

    /// A shell drives straight around the path handling `write_file` enforces,
    /// so what the command names is read and reported. String matching, not a
    /// boundary: it stops an accident and keeps `--yes` from covering the
    /// commands most worth looking at.
    fn concerns(&self, arguments: &str, root: &Path) -> Vec<String> {
        match serde_json::from_str::<TerminalArgs>(arguments) {
            Ok(args) => policy::command_concerns(&args.command, root),
            // Unparseable arguments are rejected by `preview` before this
            // matters; treating that as "no concerns" changes nothing.
            Err(_) => Vec::new(),
        }
    }

    fn call(&self, arguments: &str, root: &Path) -> Result<String, ToolError> {
        let args: TerminalArgs =
            serde_json::from_str(arguments).map_err(ToolError::InvalidArguments)?;
        run_shell(
            &args.command,
            root,
            self.timeout,
            self.backend,
            &self.sandbox,
        )
    }
}

fn run_shell(
    command: &str,
    root: &Path,
    timeout: Duration,
    backend: Backend,
    policy: &SandboxPolicy,
) -> Result<String, ToolError> {
    // The sandbox decides what the command may touch; everything below still
    // decides how it is run. Confinement does not replace the environment
    // scrubbing, the process group or the timeout, it sits under them.
    let (program, args) = sandbox::wrap(backend, policy, command);

    let mut child = Command::new(program)
        .args(args)
        .current_dir(root)
        // Start from nothing and add back only what survives scrubbing, so a
        // variable added to this process later cannot leak by being forgotten
        // here.
        .env_clear()
        .envs(policy::scrub_env(std::env::vars()))
        // No interactive input exists to give it; without this a command that
        // reads stdin would inherit the terminal and swallow the approval for
        // the next one.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so a timeout can kill what it spawned and
        // not just the shell that spawned them.
        .process_group(0)
        .spawn()
        .map_err(ToolError::Spawn)?;

    // Drained on threads: a command that fills the pipe buffer would block
    // forever if we only waited on the process.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stdout = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let stderr = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });

    let pid = child.id();
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Err(_) => break None,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            kill_group(pid);
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(EXEC_POLL);
    };

    // The readers end once every writer is closed, which the kill guarantees.
    let out = String::from_utf8_lossy(&stdout.join().unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&stderr.join().unwrap_or_default()).into_owned();

    let Some(status) = status else {
        return Err(ToolError::ExecTimeout {
            secs: timeout.as_secs(),
        });
    };

    // A non-zero exit is an answer, not a failure of the tool: a failing
    // `cargo test` is exactly what the model asked to see.
    let mut report = match status.code() {
        Some(0) => String::from("exit 0"),
        Some(code) => format!("exit {code}"),
        None => String::from("killed by a signal"),
    };
    if !out.trim().is_empty() {
        report.push_str("\n--- stdout ---\n");
        report.push_str(out.trim_end());
    }
    if !err.trim().is_empty() {
        report.push_str("\n--- stderr ---\n");
        report.push_str(err.trim_end());
    }
    if out.trim().is_empty() && err.trim().is_empty() {
        report.push_str("\n(no output)");
    }
    Ok(report)
}

/// Kill the whole group. `Child::kill` would end the shell and leave whatever
/// it started running, holding the pipes open.
fn kill_group(pid: u32) {
    // Negative pid means the process group, which `process_group(0)` set to
    // the child's own pid.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn check_write_size(content: &str) -> Result<(), ToolError> {
    match content.len() > MAX_WRITE_FILE_BYTES {
        true => Err(ToolError::WriteTooLarge {
            size: content.len(),
        }),
        false => Ok(()),
    }
}

/// Cut `output` down to the context cap, telling the model what it is seeing.
/// Without the marker the model would reason over a prefix as if it were the
/// whole file.
fn truncate_for_context(output: String, limit: usize) -> String {
    if output.len() <= limit {
        return output;
    }

    // Never split a multi-byte character: back up to a boundary.
    let mut end = limit;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }

    format!(
        "{}\n[truncated: showing {} of {} bytes]",
        &output[..end],
        end,
        output.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn read(path: &str) -> Result<String, ToolError> {
        Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES).dispatch(
            "read_file",
            &format!("{{\"path\":{}}}", json!(path)),
            &project_root(),
            &mut allow,
        )
    }

    fn dispatch(name: &str, arguments: &str) -> Result<String, ToolError> {
        Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES).dispatch(
            name,
            arguments,
            &project_root(),
            &mut allow,
        )
    }

    /// Stands in for the human saying yes.
    fn allow(_request: &Approval) -> bool {
        true
    }

    /// Stands in for the human saying no.
    fn refuse(_request: &Approval) -> bool {
        false
    }

    fn write_in(dir: &Path, path: &str, content: &str) -> Result<String, ToolError> {
        write_in_with(dir, path, content, &mut allow)
    }

    fn write_in_with(
        dir: &Path,
        path: &str,
        content: &str,
        approve: &mut dyn FnMut(&Approval) -> bool,
    ) -> Result<String, ToolError> {
        Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES).dispatch(
            "write_file",
            &format!(
                "{{\"path\":{},\"content\":{}}}",
                json!(path),
                json!(content)
            ),
            dir,
            approve,
        )
    }

    /// A throwaway project directory, so a write test never touches the real
    /// one. Named per test because these run in parallel.
    fn scratch_project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("token-write-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_a_relative_path() {
        assert!(read("Cargo.toml").unwrap().contains("name = \"token\""));
    }

    #[test]
    fn reads_an_absolute_path_outside_the_project() {
        assert!(read("/etc/hosts").unwrap().contains("localhost"));
    }

    #[test]
    fn reads_a_path_that_walks_out_and_back() {
        let task = read("../token-rust/Cargo.toml").unwrap();
        assert!(task.contains("name = \"token\""));
    }

    #[test]
    fn reports_a_missing_file() {
        assert!(matches!(read("nope.rs"), Err(ToolError::Io { .. })));
    }

    #[test]
    fn reports_a_missing_directory() {
        assert!(matches!(
            read("no/such/dir/file.rs"),
            Err(ToolError::Io { .. })
        ));
    }

    #[test]
    fn reports_a_directory() {
        assert!(matches!(read("src"), Err(ToolError::IsDirectory(_))));
    }

    #[test]
    fn reports_a_character_device() {
        assert!(matches!(
            read("/dev/null"),
            Err(ToolError::NotRegularFile(_))
        ));
    }

    #[test]
    fn reports_a_non_utf8_file() {
        let binary = read("target/debug/token");
        assert!(
            matches!(
                binary,
                Err(ToolError::NotText { .. }) | Err(ToolError::TooLarge { .. })
            ),
            "got {binary:?}"
        );
    }

    #[test]
    fn reports_an_unreadable_file() {
        // Root-owned and mode 600 on macOS; skip if the environment differs.
        if std::fs::read("/etc/sudoers").is_ok() {
            return;
        }
        assert!(matches!(read("/etc/sudoers"), Err(ToolError::Io { .. })));
    }

    /// Scratch dir under target/, which is gitignored.
    fn scratch() -> PathBuf {
        let dir = project_root().join("target/tool-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_an_empty_file() {
        let path = scratch().join("empty.txt");
        std::fs::write(&path, "").unwrap();
        assert_eq!(read(path.to_str().unwrap()).unwrap(), "");
    }

    #[test]
    fn reads_a_path_containing_spaces_and_unicode() {
        let path = scratch().join("a file — с пробелами.txt");
        std::fs::write(&path, "contents").unwrap();
        assert_eq!(read(path.to_str().unwrap()).unwrap(), "contents");
    }

    #[test]
    fn follows_a_symlink_to_a_file_outside_the_project() {
        let link = scratch().join("hosts-link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();
        assert!(read(link.to_str().unwrap()).unwrap().contains("localhost"));
    }

    #[test]
    fn reports_a_broken_symlink() {
        let link = scratch().join("broken-link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/nonexistent/target", &link).unwrap();
        assert!(matches!(
            read(link.to_str().unwrap()),
            Err(ToolError::Io { .. })
        ));
    }

    #[test]
    fn does_not_expand_a_tilde_path() {
        // The shell expands ~, but the model passes it through JSON, so it
        // arrives literally and must fail clearly rather than silently.
        assert!(matches!(read("~/.zshrc"), Err(ToolError::Io { .. })));
    }

    #[test]
    fn truncates_a_large_file_instead_of_refusing_it() {
        let path = scratch().join("big.txt");
        let size = DEFAULT_MAX_TOOL_OUTPUT_BYTES * 3;
        std::fs::write(&path, "x".repeat(size)).unwrap();

        let output = read(path.to_str().unwrap()).unwrap();
        assert!(output.starts_with("xxxx"), "content should be the head");
        assert!(
            output.contains(&format!(
                "[truncated: showing {DEFAULT_MAX_TOOL_OUTPUT_BYTES} of {size} bytes]"
            )),
            "got tail: {}",
            &output[output.len() - 60..]
        );
    }

    #[test]
    fn reads_a_file_exactly_at_the_context_cap_whole() {
        let path = scratch().join("exact.txt");
        std::fs::write(&path, "x".repeat(DEFAULT_MAX_TOOL_OUTPUT_BYTES)).unwrap();
        let output = read(path.to_str().unwrap()).unwrap();
        assert_eq!(output.len(), DEFAULT_MAX_TOOL_OUTPUT_BYTES);
        assert!(!output.contains("truncated"));
    }

    #[test]
    fn the_cap_is_configurable_for_larger_models() {
        let path = scratch().join("roomy.txt");
        std::fs::write(&path, "y".repeat(DEFAULT_MAX_TOOL_OUTPUT_BYTES * 2)).unwrap();

        let output = Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES * 4)
            .dispatch(
                "read_file",
                &format!("{{\"path\":{}}}", json!(path.to_str().unwrap())),
                &project_root(),
                &mut allow,
            )
            .unwrap();
        assert_eq!(output.len(), DEFAULT_MAX_TOOL_OUTPUT_BYTES * 2);
        assert!(!output.contains("truncated"));
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // "€" is 3 bytes; cutting at 4 would land mid-character.
        let output = truncate_for_context("a€€€".to_string(), 4);
        assert!(output.starts_with("a€"), "got {output:?}");
        assert!(output.contains("[truncated: showing 4 of 10 bytes]"));
    }

    #[test]
    fn truncation_reports_the_full_original_size() {
        let output = truncate_for_context("x".repeat(1000), 100);
        assert!(output.contains("[truncated: showing 100 of 1000 bytes]"));
    }

    #[test]
    fn reports_bad_arguments() {
        assert!(matches!(
            dispatch("read_file", r#"{"file":"x"}"#),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[test]
    fn reports_malformed_json_arguments() {
        assert!(matches!(
            dispatch("read_file", "not json"),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[test]
    fn rejects_invented_arguments() {
        // The model tried `offset`/`limit` on a large file in a real run.
        let err = dispatch(
            "read_file",
            r#"{"path":"Cargo.toml","offset":0,"limit":256}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "got {err:?}");
        assert!(err.to_string().contains("offset"), "got {err}");
    }

    #[test]
    fn unsupported_tool_message_lists_what_exists() {
        let err = dispatch("terminal", r#"{"command":"ls"}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported call: `terminal`. Available tools: read_file, write_file"
        );
    }

    #[test]
    fn model_visible_specs_come_from_the_handlers() {
        let registry = Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES);
        let specs = registry.specs();
        assert_eq!(specs.len(), 2);
        // Every advertised name must dispatch to a handler.
        for spec in &specs {
            let name = spec.name;
            assert!(
                !matches!(
                    registry.dispatch(name, "{}", &project_root(), &mut allow),
                    Err(ToolError::UnsupportedTool { .. })
                ),
                "advertised tool `{name}` is not dispatchable"
            );
        }
    }

    // --- write_file ---

    #[test]
    fn creates_a_file_and_says_so() {
        let dir = scratch_project("create");
        let result = write_in(&dir, "notes.md", "hello\n").unwrap();
        assert_eq!(result, "created notes.md (6 bytes)");
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn replaces_an_existing_file_whole() {
        let dir = scratch_project("replace");
        std::fs::write(dir.join("notes.md"), "old content here").unwrap();
        let result = write_in(&dir, "notes.md", "new").unwrap();
        assert_eq!(result, "replaced notes.md (3 bytes)");
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "new"
        );
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = scratch_project("parents");
        write_in(&dir, "src/deep/nested.rs", "fn main() {}").unwrap();
        assert!(dir.join("src/deep/nested.rs").exists());
    }

    #[test]
    fn a_refusal_writes_nothing() {
        let dir = scratch_project("refused");
        let err = write_in_with(&dir, "notes.md", "hello", &mut refuse).unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "got {err}");
        assert!(
            !dir.join("notes.md").exists(),
            "file was written despite refusal"
        );
    }

    #[test]
    fn a_refusal_does_not_destroy_what_is_there() {
        let dir = scratch_project("refused-existing");
        std::fs::write(dir.join("notes.md"), "precious").unwrap();
        let _ = write_in_with(&dir, "notes.md", "clobbered", &mut refuse);
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "precious"
        );
    }

    #[test]
    fn approval_is_only_asked_for_tools_that_change_things() {
        // read_file must never trigger a prompt.
        let mut asked = false;
        let mut spy = |_: &Approval| {
            asked = true;
            true
        };
        Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES)
            .dispatch(
                "read_file",
                r#"{"path":"Cargo.toml"}"#,
                &project_root(),
                &mut spy,
            )
            .unwrap();
        assert!(!asked, "a read asked the user for permission");
    }

    #[test]
    fn the_preview_shows_the_path_and_the_first_lines() {
        let dir = scratch_project("preview");
        let mut seen = String::new();
        let mut capture = |request: &Approval| {
            seen = request.preview.to_string();
            true
        };
        write_in_with(&dir, "notes.md", "alpha\nbeta\n", &mut capture).unwrap();
        assert!(seen.contains("notes.md"), "no path in preview: {seen}");
        assert!(seen.contains("new file"), "no summary in preview: {seen}");
        assert!(seen.contains("alpha"), "no content in preview: {seen}");
    }

    #[test]
    fn the_preview_says_when_a_file_would_be_overwritten() {
        let dir = scratch_project("preview-overwrite");
        std::fs::write(dir.join("notes.md"), "old").unwrap();
        let mut seen = String::new();
        let mut capture = |request: &Approval| {
            seen = request.preview.to_string();
            true
        };
        write_in_with(&dir, "notes.md", "new", &mut capture).unwrap();
        assert!(seen.contains("overwrite"), "overwrite not flagged: {seen}");
    }

    #[test]
    fn write_rejects_invented_arguments() {
        let dir = scratch_project("strict");
        let err = Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES)
            .dispatch(
                "write_file",
                r#"{"path":"a.txt","content":"x","mode":"append"}"#,
                &dir,
                &mut allow,
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "got {err}");
        assert!(!dir.join("a.txt").exists());
    }

    // --- terminal ---

    fn exec_registry() -> Registry {
        exec_registry_with(Duration::from_secs(10), &project_root())
    }

    /// Exercises the real backend: on macOS these tests run under Seatbelt,
    /// so a policy that breaks a shell breaks the suite.
    fn exec_registry_with(timeout: Duration, root: &Path) -> Registry {
        let mode = sandbox::SandboxMode::WorkspaceWrite;
        Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES).with_exec(
            timeout,
            sandbox::select(mode),
            SandboxPolicy::new(mode, root, false),
        )
    }

    fn run(command: &str) -> Result<String, ToolError> {
        run_in(&project_root(), command, &mut allow)
    }

    fn run_in(
        dir: &Path,
        command: &str,
        approve: &mut dyn FnMut(&Approval) -> bool,
    ) -> Result<String, ToolError> {
        exec_registry().dispatch(
            "terminal",
            &format!("{{\"command\":{}}}", json!(command)),
            dir,
            approve,
        )
    }

    #[test]
    fn a_withheld_command_tool_is_absent_rather_than_refused() {
        // `--no-exec` takes it out of the spec entirely. A tool the model
        // cannot see is one it does not keep trying to use.
        let registry = Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES);
        assert!(!registry.names().contains("terminal"));
        assert!(!registry.specs().iter().any(|s| s.name == "terminal"));

        let err = registry
            .dispatch(
                "terminal",
                r#"{"command":"echo hi"}"#,
                &project_root(),
                &mut allow,
            )
            .unwrap_err();
        assert!(
            matches!(err, ToolError::UnsupportedTool { .. }),
            "got {err}"
        );
    }

    #[test]
    fn enabling_it_puts_it_in_the_spec() {
        let registry = exec_registry();
        assert!(registry.names().contains("terminal"));
        assert!(registry.specs().iter().any(|s| s.name == "terminal"));
    }

    #[test]
    fn runs_a_command_and_reports_its_output() {
        let out = run("echo hello").unwrap();
        assert!(out.starts_with("exit 0"), "{out}");
        assert!(out.contains("hello"), "{out}");
    }

    #[test]
    fn a_failing_command_is_an_answer_not_an_error() {
        // A failing `cargo test` is exactly what the model asked to see.
        let out = run("echo oops >&2; exit 3").unwrap();
        assert!(out.contains("exit 3"), "{out}");
        assert!(out.contains("--- stderr ---"), "{out}");
        assert!(out.contains("oops"), "{out}");
    }

    #[test]
    fn runs_in_the_project_directory() {
        let dir = scratch_project("exec-cwd");
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        let out = run_in(&dir, "ls", &mut allow).unwrap();
        assert!(out.contains("marker.txt"), "{out}");
    }

    #[test]
    fn a_command_is_not_run_without_approval() {
        let dir = scratch_project("exec-refused");
        let err = run_in(&dir, "touch created.txt", &mut refuse).unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "got {err}");
        assert!(!dir.join("created.txt").exists(), "ran despite refusal");
    }

    #[test]
    fn the_preview_shows_the_command_verbatim() {
        // The human is approving the command, so it must not be summarised.
        let dir = scratch_project("exec-preview");
        let mut seen = String::new();
        let mut capture = |request: &Approval| {
            seen = request.preview.to_string();
            true
        };
        run_in(&dir, "rm -rf build && make", &mut capture).unwrap();
        assert!(seen.contains("rm -rf build && make"), "{seen}");
    }

    #[test]
    fn a_hung_command_is_killed() {
        let registry = exec_registry_with(Duration::from_millis(300), &project_root());
        let err = registry
            .dispatch(
                "terminal",
                r#"{"command":"sleep 30"}"#,
                &project_root(),
                &mut allow,
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecTimeout { .. }), "got {err}");
    }

    #[test]
    fn a_timeout_kills_the_whole_process_group() {
        // `Child::kill` would end the shell and leave the sleep running,
        // holding the pipe open and hanging the read.
        let dir = scratch_project("exec-group");
        let registry = exec_registry_with(Duration::from_millis(300), &dir);
        let started = Instant::now();
        let err = registry
            .dispatch(
                "terminal",
                r#"{"command":"sleep 30 & sleep 30"}"#,
                &dir,
                &mut allow,
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecTimeout { .. }), "got {err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}; a grandchild was left holding the pipe",
            started.elapsed()
        );
    }

    #[test]
    fn a_command_waiting_for_input_does_not_hang_the_agent() {
        let registry = exec_registry_with(Duration::from_secs(5), &project_root());
        let out = registry
            .dispatch(
                "terminal",
                r#"{"command":"cat"}"#,
                &project_root(),
                &mut allow,
            )
            .unwrap();
        // stdin is /dev/null, so `cat` sees EOF at once instead of waiting.
        assert!(out.starts_with("exit 0"), "{out}");
    }

    #[test]
    fn a_command_cannot_print_the_provider_key() {
        // The whole reason the environment is scrubbed: blocking reads of
        // .env is pointless if printenv hands the value back.
        let out = run("printenv | grep -ci 'API_KEY' || true").unwrap();
        assert!(out.contains("exit 0"), "{out}");
        let count: usize = out
            .lines()
            .last()
            .unwrap_or("0")
            .trim()
            .parse()
            .unwrap_or(999);
        assert_eq!(count, 0, "a key-shaped variable reached the child: {out}");
    }

    #[test]
    fn a_command_still_gets_a_usable_shell() {
        let out = run("test -n \"$PATH\" && echo has-path").unwrap();
        assert!(out.contains("has-path"), "scrubbing broke the shell: {out}");
    }

    #[test]
    fn a_command_naming_a_private_file_carries_a_concern() {
        let dir = scratch_project("exec-concern");
        let mut seen: Vec<String> = Vec::new();
        let mut capture = |request: &Approval| {
            seen = request.concerns.to_vec();
            false
        };
        let _ = run_in(&dir, "rm .env", &mut capture);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains(".env"), "{seen:?}");
    }

    #[test]
    fn a_command_leaving_the_project_carries_a_concern() {
        let dir = scratch_project("exec-concern-escape");
        let mut seen: Vec<String> = Vec::new();
        let mut capture = |request: &Approval| {
            seen = request.concerns.to_vec();
            false
        };
        let _ = run_in(&dir, "rm /etc/hosts", &mut capture);
        assert!(!seen.is_empty(), "an escape was not flagged");
    }

    #[test]
    fn ordinary_work_carries_no_concern() {
        // Flagging routine commands would teach the user to stop reading.
        let dir = scratch_project("exec-no-concern");
        let mut seen: Vec<String> = Vec::new();
        let mut capture = |request: &Approval| {
            seen = request.concerns.to_vec();
            true
        };
        run_in(&dir, "echo hi", &mut capture).unwrap();
        assert!(seen.is_empty(), "{seen:?}");
    }

    #[test]
    fn a_write_carries_no_concern_because_its_paths_are_already_confined() {
        let dir = scratch_project("write-no-concern");
        let mut seen: Vec<String> = Vec::new();
        let mut capture = |request: &Approval| {
            seen = request.concerns.to_vec();
            true
        };
        write_in_with(&dir, "notes.md", "hi", &mut capture).unwrap();
        assert!(seen.is_empty(), "{seen:?}");
    }

    #[test]
    fn exec_rejects_invented_arguments() {
        let err = exec_registry()
            .dispatch(
                "terminal",
                r#"{"command":"echo hi","timeout":5}"#,
                &project_root(),
                &mut allow,
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "got {err}");
    }

    // --- the sandbox, where there is one ---

    /// These assert what the kernel refuses. On a platform with no backend
    /// there is nothing to assert, so they stand down rather than pass
    /// vacuously and imply a boundary that is not there.
    fn sandboxed() -> bool {
        sandbox::select(sandbox::SandboxMode::WorkspaceWrite) != Backend::None
    }

    #[test]
    fn the_sandbox_refuses_a_write_outside_the_project() {
        if !sandboxed() {
            return;
        }
        let dir = scratch_project("sb-escape");
        // Somewhere outside both the project and the temp grant.
        let target = format!("{}/token-sb-escaped.txt", std::env::var("HOME").unwrap());
        let _ = std::fs::remove_file(&target);

        // No `..` in the command, so `policy.rs` sees nothing to flag: this
        // is the kernel refusing, not the screening.
        let out = run_in(&dir, &format!("echo pwned > {target}"), &mut allow).unwrap();

        assert!(!out.starts_with("exit 0"), "the write succeeded: {out}");
        assert!(!Path::new(&target).exists(), "wrote outside the project");
    }

    #[test]
    fn the_sandbox_refuses_to_read_a_dotenv_the_screening_would_miss() {
        if !sandboxed() {
            return;
        }
        // The hole the sandbox exists to close: a shell walks around
        // `policy.rs`, and quoting the name walks around the screening too.
        let dir = scratch_project("sb-dotenv");
        std::fs::write(dir.join(".env"), "SECRET=real_value_here").unwrap();

        let out = run_in(&dir, r#"cat ".e""nv""#, &mut allow).unwrap();
        assert!(
            !out.contains("real_value_here"),
            "the secret was read: {out}"
        );
    }

    #[test]
    fn the_sandbox_leaves_the_example_dotenv_readable() {
        if !sandboxed() {
            return;
        }
        let dir = scratch_project("sb-dotenv-example");
        std::fs::write(dir.join(".env.example"), "API_KEY=your_key_here").unwrap();

        let out = run_in(&dir, "cat .env.example", &mut allow).unwrap();
        assert!(out.contains("your_key_here"), "{out}");
    }

    #[test]
    fn the_sandbox_allows_ordinary_work_in_the_project() {
        // A sandbox that breaks the toolchain is one the user switches off.
        if !sandboxed() {
            return;
        }
        let dir = scratch_project("sb-ordinary");
        let out = run_in(&dir, "echo hi > made.txt && cat made.txt", &mut allow).unwrap();
        assert!(out.starts_with("exit 0"), "{out}");
        assert!(out.contains("hi"), "{out}");
        assert!(dir.join("made.txt").exists());
    }

    #[test]
    fn read_only_mode_refuses_a_write_inside_the_project() {
        if !sandboxed() {
            return;
        }
        let dir = scratch_project("sb-readonly");
        let mode = sandbox::SandboxMode::ReadOnly;
        let registry = Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES).with_exec(
            Duration::from_secs(10),
            sandbox::select(mode),
            SandboxPolicy::new(mode, &dir, false),
        );
        let out = registry
            .dispatch(
                "terminal",
                r#"{"command":"echo x > nope.txt"}"#,
                &dir,
                &mut allow,
            )
            .unwrap();
        assert!(!out.starts_with("exit 0"), "{out}");
        assert!(!dir.join("nope.txt").exists());
    }

    // --- the privacy boundary ---

    #[test]
    fn refuses_to_write_dotenv() {
        let dir = scratch_project("dotenv-write");
        std::fs::write(dir.join(".env"), "SARVAM_API_KEY=real_secret").unwrap();

        let err = write_in(&dir, ".env", "SARVAM_API_KEY=whatever").unwrap_err();
        assert!(matches!(err, ToolError::Private { .. }), "got {err}");
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "SARVAM_API_KEY=real_secret",
            "the real .env was modified"
        );
    }

    #[test]
    fn refuses_to_read_dotenv() {
        let dir = scratch_project("dotenv-read");
        std::fs::write(dir.join(".env"), "SARVAM_API_KEY=real_secret").unwrap();

        let err = Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES)
            .dispatch("read_file", r#"{"path":".env"}"#, &dir, &mut allow)
            .unwrap_err();
        assert!(matches!(err, ToolError::Private { .. }), "got {err}");
        assert!(!err.to_string().contains("real_secret"));
    }

    #[test]
    fn the_refusal_never_asks_the_user_about_a_secret() {
        // A private path must be refused before anyone is prompted: a preview
        // of .env would print the secret it exists to protect.
        let dir = scratch_project("dotenv-no-prompt");
        std::fs::write(dir.join(".env"), "KEY=secret").unwrap();
        let mut asked = false;
        let mut spy = |_: &Approval| {
            asked = true;
            true
        };
        let _ = write_in_with(&dir, ".env", "KEY=other", &mut spy);
        assert!(!asked, "the user was prompted about a private file");
    }

    #[test]
    fn allows_writing_the_example_dotenv() {
        // The documented way out: name the variable, never its value.
        let dir = scratch_project("dotenv-example");
        write_in(&dir, ".env.example", "SARVAM_API_KEY=your_key_here\n").unwrap();
        assert!(dir.join(".env.example").exists());
    }

    #[test]
    fn refuses_to_put_a_real_secret_in_the_example_dotenv() {
        // Blocking .env alone just moves the secret into the file that gets
        // committed. This is the live failure that made the check necessary.
        let dir = scratch_project("example-leak");
        let err = write_in(
            &dir,
            ".env.example",
            "# Copy to .env\n\nSTRIPE_API_KEY=sk_live_abc123\n",
        )
        .unwrap_err();
        assert!(
            matches!(err, ToolError::SecretInExample { .. }),
            "got {err}"
        );
        assert!(!dir.join(".env.example").exists());
    }

    #[test]
    fn the_leak_refusal_tells_the_model_what_to_write_instead() {
        let dir = scratch_project("example-leak-msg");
        let err = write_in(&dir, ".env.example", "API_KEY=sk_live_abc123").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("placeholder"), "unhelpful: {message}");
        assert!(
            message.contains("API_KEY"),
            "does not name the key: {message}"
        );
    }

    #[test]
    fn allows_the_example_dotenv_with_placeholders() {
        let dir = scratch_project("example-ok");
        write_in(
            &dir,
            ".env.example",
            "OPENAI_API_KEY=sk-your_key_here\nPORT=3000\n",
        )
        .unwrap();
        assert!(dir.join(".env.example").exists());
    }

    #[test]
    fn refuses_key_material_and_credentials() {
        let dir = scratch_project("secrets");
        for path in [".env.local", "server.pem", "deploy.key", ".netrc"] {
            let err = write_in(&dir, path, "x").unwrap_err();
            assert!(matches!(err, ToolError::Private { .. }), "{path} got {err}");
        }
    }

    #[test]
    fn refuses_to_write_inside_the_git_directory() {
        let dir = scratch_project("git");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let err = write_in(&dir, ".git/config", "[core]").unwrap_err();
        assert!(matches!(err, ToolError::Private { .. }), "got {err}");
    }

    // --- write scope ---

    #[test]
    fn refuses_to_write_outside_the_project_by_walking_up() {
        let dir = scratch_project("escape-relative");
        let err = write_in(&dir, "../escaped.txt", "x").unwrap_err();
        assert!(matches!(err, ToolError::OutsideRoot { .. }), "got {err}");
        assert!(!dir.parent().unwrap().join("escaped.txt").exists());
    }

    #[test]
    fn refuses_to_write_an_absolute_path_outside_the_project() {
        let dir = scratch_project("escape-absolute");
        let outside = std::env::temp_dir().join("token-should-not-exist.txt");
        let _ = std::fs::remove_file(&outside);

        let err = write_in(&dir, outside.to_str().unwrap(), "x").unwrap_err();
        assert!(matches!(err, ToolError::OutsideRoot { .. }), "got {err}");
        assert!(!outside.exists(), "wrote outside the project");
    }

    #[test]
    fn a_refused_escape_creates_no_directory_outside_the_project() {
        // Containment is decided lexically, before anything is made. Deciding
        // it after create_dir_all would leave this directory behind.
        let dir = scratch_project("escape-mkdir");
        let outside = dir.parent().unwrap().join("token-should-not-be-created");
        let _ = std::fs::remove_dir_all(&outside);

        let err = write_in(&dir, "../token-should-not-be-created/f.txt", "x").unwrap_err();
        assert!(matches!(err, ToolError::OutsideRoot { .. }), "got {err}");
        assert!(!outside.exists(), "created a directory outside the project");
    }

    #[test]
    fn refuses_to_write_through_a_symlink_that_leaves_the_project() {
        // Following it is how an approved-looking path escapes a checked dir.
        let dir = scratch_project("escape-symlink");
        let outside = std::env::temp_dir().join("token-symlink-target.txt");
        std::fs::write(&outside, "original").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("innocent.txt")).unwrap();

        let err = write_in(&dir, "innocent.txt", "clobbered").unwrap_err();
        assert!(matches!(err, ToolError::IsSymlink { .. }), "got {err}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
    }

    #[test]
    fn refuses_to_write_over_a_directory() {
        let dir = scratch_project("over-dir");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let err = write_in(&dir, "src", "x").unwrap_err();
        assert!(matches!(err, ToolError::IsDirectory(_)), "got {err}");
    }

    #[test]
    fn a_path_that_walks_out_and_back_stays_inside() {
        let dir = scratch_project("out-and-back");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        write_in(&dir, "src/../notes.md", "fine").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "fine"
        );
    }

    #[test]
    fn reports_content_over_the_write_limit() {
        let dir = scratch_project("too-big");
        let huge = "x".repeat(MAX_WRITE_FILE_BYTES + 1);
        let err = write_in(&dir, "big.txt", &huge).unwrap_err();
        assert!(matches!(err, ToolError::WriteTooLarge { .. }), "got {err}");
        assert!(!dir.join("big.txt").exists());
    }

    #[test]
    fn reports_an_unknown_tool() {
        assert!(matches!(
            dispatch("delete_file", "{}"),
            Err(ToolError::UnsupportedTool { .. })
        ));
    }
}
