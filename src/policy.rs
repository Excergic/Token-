//! What the agent may never touch.
//!
//! A name-based deny list, applied to reads and writes alike, so a secret is
//! refused before it is opened rather than after it is in the transcript. Once
//! a key reaches the model it has left this machine and been recorded in the
//! session database, so the check has to happen here and not in the answer.
//!
//! This is a blocklist, not a sandbox. It stops the agent naming a secret it
//! knows about; it does not confine it to the project. Write scope does that,
//! and it is enforced separately in `tools.rs`.

use std::path::{Component, Path};

/// Directories whose contents are private wherever they appear in a path.
/// `.config` is deliberately absent: projects keep ordinary settings there
/// and blocking it would cost more than it protects.
const PRIVATE_DIRS: [&str; 4] = [".git", ".ssh", ".gnupg", ".aws"];

/// Exact file names that are private.
const PRIVATE_NAMES: [&str; 9] = [
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    "credentials",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// Extensions that mean key material.
const PRIVATE_EXTENSIONS: [&str; 6] = ["pem", "key", "p12", "pfx", "keystore", "jks"];

/// Dotenv files that carry no values and exist to be shared. These stay
/// readable and writable: they are how the agent documents a variable it must
/// not set.
const SHAREABLE_ENV_FILES: [&str; 4] =
    [".env.example", ".env.sample", ".env.template", ".env.dist"];

/// Key names whose value is a secret. Substring match, so `STRIPE_API_KEY`
/// and `DB_PASSWORD` are both caught.
const SECRET_KEY_HINTS: [&str; 11] = [
    "KEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE",
    "AUTH",
    "DSN",
    "SIGNATURE",
    "CERT",
];

/// Marks a value as deliberately fake. An allow list, not a deny list: a
/// value has to look like a placeholder to be let through, because guessing
/// which strings are real secrets is the losing direction to be wrong in.
const PLACEHOLDER_HINTS: [&str; 12] = [
    "your",
    "example",
    "placeholder",
    "changeme",
    "change_me",
    "xxx",
    "todo",
    "here",
    "dummy",
    "replace",
    "insert",
    "fake",
];

/// Whether this path is a dotenv file meant to be shared, and therefore one
/// whose values are read by people who did not write them.
pub fn is_shareable_env_file(path: &Path) -> bool {
    path.file_name()
        .map(|name| SHAREABLE_ENV_FILES.contains(&name.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(false)
}

/// The first secret-looking key in a shareable dotenv whose value is not a
/// placeholder, or `None` if the content is safe to commit.
///
/// Refusing `.env` alone is not enough: told to record a secret, a model will
/// put the real value into `.env.example` instead, which is the file that
/// gets committed. `.env` is gitignored; this one is not.
pub fn committed_secret(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim().trim_matches(['"', '\'']));
        if value.is_empty() {
            continue;
        }

        let upper = key.to_uppercase();
        if !SECRET_KEY_HINTS.iter().any(|hint| upper.contains(hint)) {
            continue;
        }
        let lower = value.to_lowercase();
        if PLACEHOLDER_HINTS.iter().any(|hint| lower.contains(hint)) {
            continue;
        }
        if value.starts_with('<') || value.starts_with("${") {
            continue;
        }
        return Some(key.to_string());
    }
    None
}

/// What about a shell command deserves a second look, in the user's words.
///
/// `run_command` is a shell: it drives straight around the path handling that
/// `read_file` and `write_file` enforce, and `rm .env` is not a write the
/// registry can refuse. This reads the command text and names what it sees,
/// so the approval prompt can say why it is worth reading carefully.
///
/// **This is not a security boundary and must never be sold as one.** It is
/// string matching on a shell command: `cat .e""nv`, `$HOME/.ssh/id_rsa` and
/// `eval` defeat it without effort. It exists to stop an accident, and to
/// make `--yes` stop short of the commands most worth seeing. The approval
/// prompt is the real gate.
pub fn command_concerns(command: &str, root: &Path) -> Vec<String> {
    let mut concerns: Vec<String> = Vec::new();
    let mut note = |concern: String| {
        if !concerns.contains(&concern) {
            concerns.push(concern);
        }
    };

    for token in path_like_tokens(command) {
        // A flag is not a path. `-rf` should not be mistaken for one.
        if token.starts_with('-') {
            continue;
        }
        let path = Path::new(&token);

        if let Some(reason) = private_reason(path) {
            note(format!("`{token}` is normally off limits: {reason}"));
            continue;
        }
        if token.starts_with('~') {
            note(format!(
                "`{token}` is in the home directory, outside the project"
            ));
            continue;
        }
        if path.is_absolute() && !path.starts_with(root) {
            note(format!("`{token}` is outside the project"));
            continue;
        }
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            note(format!("`{token}` reaches outside the project"));
        }
    }
    concerns
}

/// Split a command into the words that might be paths. Deliberately crude:
/// it splits on whitespace and shell punctuation and looks no further, since
/// anything cleverer would invite belief in a guarantee this cannot give.
fn path_like_tokens(command: &str) -> Vec<String> {
    command
        .split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    ';' | '|' | '&' | '(' | ')' | '<' | '>' | '`' | '"' | '\'' | '='
                )
        })
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

/// Substrings that mark an environment variable as carrying a credential.
/// Matched case-insensitively against the name, so `SARVAM_API_KEY`,
/// `GITHUB_TOKEN` and `aws_secret_access_key` all go.
const SECRET_ENV_HINTS: [&str; 8] = [
    "KEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "AUTH",
    "CREDENTIAL",
    "PRIVATE",
];

/// The environment a child process is allowed to inherit.
///
/// This process holds the provider key: it is read from `.env` at startup and
/// lives in our own environment for the whole run. A command the model asks
/// for would inherit it by default, and `env` or `printenv` would hand it
/// straight back into the transcript. Blocking reads of `.env` is pointless
/// if the value is sitting in the child's environment anyway.
///
/// A block list rather than an allow list, so `PATH`, `HOME` and the rest of
/// a working shell survive. It takes the variables as an argument rather than
/// reading the environment itself, so it can be tested without touching the
/// real one.
pub fn scrub_env<I>(vars: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    vars.into_iter()
        .filter(|(name, _)| !is_secret_env(name))
        .collect()
}

fn is_secret_env(name: &str) -> bool {
    let upper = name.to_uppercase();
    SECRET_ENV_HINTS.iter().any(|hint| upper.contains(hint))
}

/// Why a path is off limits, or `None` if it is fine to touch.
///
/// The reason is returned rather than a bare bool because it goes back to the
/// model as a tool failure: it has to learn what it may do instead.
pub fn private_reason(path: &Path) -> Option<&'static str> {
    for component in path.components() {
        let component = component.as_os_str().to_string_lossy();
        if PRIVATE_DIRS.contains(&component.as_ref()) {
            return Some("it is inside a private directory");
        }
    }

    let name = match path.file_name() {
        Some(name) => name.to_string_lossy().to_lowercase(),
        None => return None,
    };

    if SHAREABLE_ENV_FILES.contains(&name.as_str()) {
        return None;
    }
    if name == ".env" || name.starts_with(".env.") {
        return Some("dotenv files hold secrets; use .env.example instead");
    }
    if PRIVATE_NAMES.contains(&name.as_str()) {
        return Some("it is a credentials file");
    }
    if let Some(extension) = path.extension() {
        let extension = extension.to_string_lossy().to_lowercase();
        if PRIVATE_EXTENSIONS.contains(&extension.as_ref()) {
            return Some("it is key material");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn denied(path: &str) -> bool {
        private_reason(&PathBuf::from(path)).is_some()
    }

    #[test]
    fn dotenv_is_private_in_every_form() {
        assert!(denied(".env"));
        assert!(denied("/Users/someone/project/.env"));
        assert!(denied(".env.local"));
        assert!(denied(".env.production"));
        assert!(denied("config/.env.staging"));
    }

    #[test]
    fn the_example_dotenv_stays_open() {
        // The whole point: the agent documents a variable without setting it.
        assert!(!denied(".env.example"));
        assert!(!denied("/Users/someone/project/.env.example"));
        assert!(!denied(".env.sample"));
        assert!(!denied(".env.template"));
    }

    #[test]
    fn the_dotenv_message_points_somewhere_useful() {
        let reason = private_reason(&PathBuf::from(".env")).unwrap();
        assert!(reason.contains(".env.example"), "unhelpful: {reason}");
    }

    #[test]
    fn key_material_is_private() {
        assert!(denied("server.pem"));
        assert!(denied("certs/private.KEY"));
        assert!(denied("~/.ssh/id_ed25519"));
        assert!(denied("bundle.p12"));
    }

    #[test]
    fn credentials_files_are_private() {
        assert!(denied(".netrc"));
        assert!(denied("/home/x/.aws/credentials"));
        assert!(denied(".git-credentials"));
    }

    #[test]
    fn private_directories_are_private_at_any_depth() {
        assert!(denied(".git/config"));
        assert!(denied("vendor/.git/HEAD"));
        assert!(denied("/Users/x/.ssh/known_hosts"));
    }

    #[test]
    fn ordinary_source_files_are_left_alone() {
        assert!(!denied("src/main.rs"));
        assert!(!denied("Cargo.toml"));
        assert!(!denied("README.md"));
        assert!(!denied("docs/environment.md"));
        // Not a dotenv: the name merely contains "env".
        assert!(!denied("src/environment.rs"));
        assert!(!denied("tests/env_test.rs"));
    }

    #[test]
    fn a_real_looking_secret_in_an_example_file_is_caught() {
        // The exact failure seen from a live run: told to record a secret and
        // refused .env, the model wrote the real value into .env.example.
        let leaked = "# Copy to .env\n\nSTRIPE_API_KEY=sk_live_abc123\n";
        assert_eq!(committed_secret(leaked).as_deref(), Some("STRIPE_API_KEY"));
    }

    #[test]
    fn placeholders_are_what_an_example_file_is_for() {
        let fine = "OPENAI_API_KEY=sk-your_key_here\nSARVAM_API_KEY=sk_your_key_here\n";
        assert_eq!(committed_secret(fine), None);
        assert_eq!(committed_secret("DB_PASSWORD=<your password>"), None);
        assert_eq!(committed_secret("AUTH_TOKEN=${AUTH_TOKEN}"), None);
        assert_eq!(committed_secret("API_KEY="), None);
        assert_eq!(committed_secret("SECRET_KEY=changeme"), None);
    }

    #[test]
    fn ordinary_settings_are_not_secrets() {
        // An example file is full of these; refusing them would make the
        // check worthless in practice.
        assert_eq!(committed_secret("PORT=3000"), None);
        assert_eq!(committed_secret("NODE_ENV=development"), None);
        assert_eq!(committed_secret("DEBUG=true"), None);
        assert_eq!(committed_secret("# API_KEY=sk_live_real"), None);
    }

    #[test]
    fn quoting_a_secret_does_not_hide_it() {
        assert_eq!(
            committed_secret("API_KEY=\"sk_live_abc123\"").as_deref(),
            Some("API_KEY")
        );
    }

    #[test]
    fn only_shareable_env_files_are_checked_this_way() {
        assert!(is_shareable_env_file(Path::new(".env.example")));
        assert!(is_shareable_env_file(Path::new("/p/.env.sample")));
        assert!(!is_shareable_env_file(Path::new("src/main.rs")));
        assert!(!is_shareable_env_file(Path::new(".env")));
    }

    fn scrubbed(vars: &[(&str, &str)]) -> Vec<String> {
        scrub_env(
            vars.iter()
                .map(|(name, value)| (name.to_string(), value.to_string())),
        )
        .into_iter()
        .map(|(name, _)| name)
        .collect()
    }

    #[test]
    fn a_child_never_inherits_the_provider_key() {
        // The reason this function exists: `printenv` would otherwise hand
        // the key back into the transcript.
        let kept = scrubbed(&[
            ("SARVAM_API_KEY", "sk_real"),
            ("OPENAI_API_KEY", "sk-real"),
            ("PATH", "/usr/bin"),
        ]);
        assert_eq!(kept, vec!["PATH"]);
    }

    #[test]
    fn strips_credentials_by_any_common_name() {
        let kept = scrubbed(&[
            ("GITHUB_TOKEN", "x"),
            ("AWS_SECRET_ACCESS_KEY", "x"),
            ("DB_PASSWORD", "x"),
            ("SSH_AUTH_SOCK", "/tmp/s"),
            ("aws_secret_access_key", "x"),
            ("MY_PRIVATE_CERT", "x"),
        ]);
        assert!(kept.is_empty(), "leaked: {kept:?}");
    }

    #[test]
    fn a_working_shell_survives() {
        // Scrubbing everything would be safe and useless.
        let kept = scrubbed(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/Users/x"),
            ("LANG", "en_US.UTF-8"),
            ("TERM", "xterm"),
            ("PWD", "/p"),
            ("SHELL", "/bin/zsh"),
        ]);
        assert_eq!(kept.len(), 6, "dropped something a shell needs: {kept:?}");
    }

    fn concerns(command: &str) -> Vec<String> {
        command_concerns(command, Path::new("/project"))
    }

    #[test]
    fn a_command_naming_a_private_file_is_flagged() {
        assert!(!concerns("rm .env").is_empty());
        assert!(!concerns("cat ~/.ssh/id_rsa").is_empty());
        assert!(!concerns("cp server.pem /tmp/x").is_empty());
    }

    #[test]
    fn a_command_leaving_the_project_is_flagged() {
        assert!(!concerns("rm /etc/hosts").is_empty());
        assert!(!concerns("rm ../outside.txt").is_empty());
        assert!(!concerns("rm -rf /").is_empty());
        assert!(!concerns("cp notes.md ~/Desktop").is_empty());
    }

    #[test]
    fn ordinary_project_work_is_not_flagged() {
        // Flagging these would train the user to approve without reading.
        assert!(concerns("cargo test").is_empty());
        assert!(concerns("ls src").is_empty());
        assert!(concerns("cat src/main.rs").is_empty());
        assert!(concerns("grep -rn TODO .").is_empty());
        assert!(concerns("rustc hello.rs -o hello && ./hello").is_empty());
        assert!(concerns("rm -rf target").is_empty());
    }

    #[test]
    fn a_path_inside_the_project_is_not_flagged_for_being_absolute() {
        assert!(concerns("cat /project/src/main.rs").is_empty());
    }

    #[test]
    fn the_concern_says_which_word_caused_it() {
        let flagged = concerns("rm -rf build && cat .env");
        assert_eq!(flagged.len(), 1, "{flagged:?}");
        assert!(flagged[0].contains(".env"), "{flagged:?}");
    }

    #[test]
    fn case_does_not_get_round_it() {
        assert!(denied(".ENV"));
        assert!(denied("KEY.PEM"));
    }
}
