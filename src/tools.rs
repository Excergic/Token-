//! Tools the model can call.
//!
//! The model is never trusted to name its own tools. Every tool is a handler
//! that carries both its schema and its implementation, the registry builds
//! the model-visible list from those same handlers, and dispatch is an exact
//! lookup: an invented name cannot reach any code.

use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Tier 1, the filesystem limit: the largest file we will read into memory at
/// all. This only stops absurd reads; anything under it is read and then
/// truncated to the context cap below.
const MAX_READ_FILE_BYTES: u64 = 512 * 1024 * 1024;

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
}

/// A tool the model can call. One implementation carries both the schema the
/// model sees and the code that runs, so the two cannot drift apart.
pub trait Tool {
    fn name(&self) -> &'static str;
    fn spec(&self) -> Value;
    fn call(&self, arguments: &str, root: &Path) -> Result<String, ToolError>;
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
            tools: vec![Box::new(ReadFile)],
            max_output_bytes,
        }
    }

    /// The tool list sent to the model, generated from the handlers above.
    pub fn specs(&self) -> Vec<Value> {
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
    pub fn dispatch(&self, name: &str, arguments: &str, root: &Path) -> Result<String, ToolError> {
        match self.tools.iter().find(|tool| tool.name() == name) {
            // Every tool's output passes through the context cap, not just
            // this one: an exec tool's output would need the same guard.
            Some(tool) => tool
                .call(arguments, root)
                .map(|output| truncate_for_context(output, self.max_output_bytes)),
            None => Err(ToolError::UnsupportedTool {
                name: name.to_string(),
                available: self.names(),
            }),
        }
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

    fn spec(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": "Read a UTF-8 text file and return its contents. Reads from \
the start of the file and takes no offset or limit; output longer than the tool output \
limit is truncated, and the truncation is stated in the result.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file. Relative paths resolve against the project root, e.g. src/main.rs; absolute paths such as /etc/hosts are also allowed."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        })
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
fn resolve(path: &str, root: &Path) -> Result<PathBuf, ToolError> {
    root.join(path)
        .canonicalize()
        .map_err(|source| ToolError::Io {
            path: path.to_string(),
            source,
        })
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
        )
    }

    fn dispatch(name: &str, arguments: &str) -> Result<String, ToolError> {
        Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES).dispatch(name, arguments, &project_root())
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
        assert!(matches!(read("no/such/dir/file.rs"), Err(ToolError::Io { .. })));
    }

    #[test]
    fn reports_a_directory() {
        assert!(matches!(read("src"), Err(ToolError::IsDirectory(_))));
    }

    #[test]
    fn reports_a_character_device() {
        assert!(matches!(read("/dev/null"), Err(ToolError::NotRegularFile(_))));
    }

    #[test]
    fn reports_a_non_utf8_file() {
        let binary = read("target/debug/token");
        assert!(
            matches!(binary, Err(ToolError::NotText { .. }) | Err(ToolError::TooLarge { .. })),
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
        assert!(matches!(read(link.to_str().unwrap()), Err(ToolError::Io { .. })));
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
        let err = dispatch("read_file", r#"{"path":"Cargo.toml","offset":0,"limit":256}"#)
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "got {err:?}");
        assert!(err.to_string().contains("offset"), "got {err}");
    }

    #[test]
    fn unsupported_tool_message_lists_what_exists() {
        let err = dispatch("terminal", r#"{"command":"ls"}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported call: `terminal`. Available tools: read_file"
        );
    }

    #[test]
    fn model_visible_specs_come_from_the_handlers() {
        let registry = Registry::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES);
        let specs = registry.specs();
        assert_eq!(specs.len(), 1);
        // Every advertised name must dispatch to a handler.
        for spec in &specs {
            let name = spec["function"]["name"].as_str().unwrap();
            assert!(
                !matches!(
                    registry.dispatch(name, "{}", &project_root()),
                    Err(ToolError::UnsupportedTool { .. })
                ),
                "advertised tool `{name}` is not dispatchable"
            );
        }
    }

    #[test]
    fn reports_an_unknown_tool() {
        assert!(matches!(
            dispatch("write_file", "{}"),
            Err(ToolError::UnsupportedTool { .. })
        ));
    }
}
