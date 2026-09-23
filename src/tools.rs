//! Tools the model can call.
//!
//! The model is never trusted to name its own tools. Every tool is a handler
//! that carries both its schema and its implementation, the registry builds
//! the model-visible list from those same handlers, and dispatch is an exact
//! lookup: an invented name cannot reach any code.

use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

use crate::policy;

/// Tier 1, the filesystem limit: the largest file we will read into memory at
/// all. This only stops absurd reads; anything under it is read and then
/// truncated to the context cap below.
const MAX_READ_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// The largest file the agent may write. Content comes from the model, so
/// this is a backstop against a runaway generation, not a workflow limit.
const MAX_WRITE_FILE_BYTES: usize = 8 * 1024 * 1024;

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
        approve: &mut dyn FnMut(&str, &str) -> bool,
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
            if !approve(tool.name(), &preview) {
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
    fn allow(_tool: &str, _preview: &str) -> bool {
        true
    }

    /// Stands in for the human saying no.
    fn refuse(_tool: &str, _preview: &str) -> bool {
        false
    }

    fn write_in(dir: &Path, path: &str, content: &str) -> Result<String, ToolError> {
        write_in_with(dir, path, content, &mut allow)
    }

    fn write_in_with(
        dir: &Path,
        path: &str,
        content: &str,
        approve: &mut dyn FnMut(&str, &str) -> bool,
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
        let mut spy = |_: &str, _: &str| {
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
        let mut capture = |_: &str, preview: &str| {
            seen = preview.to_string();
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
        let mut capture = |_: &str, preview: &str| {
            seen = preview.to_string();
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
        let mut spy = |_: &str, _: &str| {
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
