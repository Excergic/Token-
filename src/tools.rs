//! Tools the model can call. Each tool is a pure function: arguments in,
//! string out. Nothing here knows about conversations or the LLM.

use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

/// Largest file we will hand back to the model, in bytes.
const MAX_FILE_BYTES: usize = 64 * 1024;

/// Errors a tool reports back to the model. These are not fatal: the runtime
/// turns them into a tool result so the model can correct itself.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(serde_json::Error),
    #[error("path is outside the project directory")]
    OutsideProject,
    #[error("could not read `{path}`: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("file is {size} bytes, larger than the {MAX_FILE_BYTES} byte limit")]
    TooLarge { size: usize },
}

/// The tool definitions sent to the model so it knows what it can call.
pub fn definitions() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a UTF-8 text file from the project directory and return its contents.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the project root, e.g. src/main.rs"
                    }
                },
                "required": ["path"]
            }
        }
    })]
}

/// Run a tool by the name the model used. `root` is the project directory
/// every path is resolved against and confined to.
pub fn run(name: &str, arguments: &str, root: &Path) -> Result<String, ToolError> {
    match name {
        "read_file" => {
            let args: ReadFileArgs =
                serde_json::from_str(arguments).map_err(ToolError::InvalidArguments)?;
            read_file(&args.path, root)
        }
        other => Err(ToolError::UnknownTool(other.to_string())),
    }
}

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
}

fn read_file(path: &str, root: &Path) -> Result<String, ToolError> {
    let resolved = resolve_in_root(path, root)?;

    let bytes = std::fs::read(&resolved).map_err(|source| ToolError::Io {
        path: path.to_string(),
        source,
    })?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ToolError::TooLarge { size: bytes.len() });
    }

    String::from_utf8(bytes).map_err(|err| ToolError::Io {
        path: path.to_string(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, err),
    })
}

/// Resolve `path` against `root` and confirm the result stays inside it.
///
/// Containment is checked twice: lexically first, so an escaping path is
/// rejected whether or not it exists, then again after canonicalising, which
/// is what catches a symlink pointing out of the project.
fn resolve_in_root(path: &str, root: &Path) -> Result<PathBuf, ToolError> {
    let root = root.canonicalize().map_err(|source| ToolError::Io {
        path: root.display().to_string(),
        source,
    })?;

    let lexical = normalize(&root.join(path));
    if !lexical.starts_with(&root) {
        return Err(ToolError::OutsideProject);
    }

    let resolved = lexical.canonicalize().map_err(|source| ToolError::Io {
        path: path.to_string(),
        source,
    })?;
    if !resolved.starts_with(&root) {
        return Err(ToolError::OutsideProject);
    }

    Ok(resolved)
}

/// Resolve `.` and `..` textually, without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn project_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn reads_a_file_inside_the_project() {
        let contents = run("read_file", r#"{"path":"Cargo.toml"}"#, &project_root()).unwrap();
        assert!(contents.contains("name = \"token\""));
    }

    #[test]
    fn rejects_a_path_that_escapes_the_project() {
        let err = run("read_file", r#"{"path":"../../etc/hosts"}"#, &project_root()).unwrap_err();
        assert!(matches!(err, ToolError::OutsideProject), "got {err:?}");
    }

    #[test]
    fn rejects_an_escaping_path_even_when_the_target_exists() {
        let err = run(
            "read_file",
            r#"{"path":"../../../../../../etc/hosts"}"#,
            &project_root(),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::OutsideProject), "got {err:?}");
    }

    #[test]
    fn rejects_an_absolute_path_outside_the_project() {
        let err = run("read_file", r#"{"path":"/etc/hosts"}"#, &project_root()).unwrap_err();
        assert!(matches!(err, ToolError::OutsideProject), "got {err:?}");
    }

    #[test]
    fn reports_a_missing_file() {
        let err = run("read_file", r#"{"path":"nope.rs"}"#, &project_root()).unwrap_err();
        assert!(matches!(err, ToolError::Io { .. }), "got {err:?}");
    }

    #[test]
    fn reports_bad_arguments() {
        let err = run("read_file", r#"{"file":"x"}"#, &project_root()).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "got {err:?}");
    }

    #[test]
    fn reports_an_unknown_tool() {
        let err = run("write_file", "{}", &project_root()).unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(_)), "got {err:?}");
    }
}
