//! OS-enforced confinement for commands the model runs.
//!
//! Everything else in this project asks the model to behave, or inspects what
//! it asked for. This does neither: the kernel refuses. `policy.rs` screens a
//! command's text and can be walked past by a quoted or variable-built path;
//! a write denied here fails whatever the command looks like.
//!
//! Platform-specific by nature. macOS goes through `sandbox-exec` and a
//! Seatbelt policy; Linux would want bubblewrap and Landlock, which is why
//! `Backend` is an enum and the policy is built separately from the wrapping.
//! On a platform with no backend the command runs unconfined and the caller
//! is told, rather than being left to assume a boundary that is not there.

use std::path::{Path, PathBuf};

/// How much a command is allowed to do, as the user asks for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SandboxMode {
    /// No confinement. The command can do anything the user can.
    Off,
    /// No writes anywhere. Remote network denied; loopback stays open.
    /// Enough to build nothing and inspect everything.
    ReadOnly,
    /// Writes confined to the project and the temporary directories a
    /// toolchain needs. Remote network denied; loopback stays open.
    WorkspaceWrite,
}

/// What is actually going to enforce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Nothing does. Either the mode is `Off` or the platform has no support.
    None,
    /// `sandbox-exec` with a Seatbelt policy.
    MacosSeatbelt,
}

/// The rules a backend renders. Kept separate from the rendering so the
/// decisions can be read, and tested, without a platform in the way.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub writable_roots: Vec<PathBuf>,
    pub allow_network: bool,
}

/// Paths never readable inside a sandbox, as regexes over the whole path.
///
/// Narrower than `policy.rs` on purpose. That list includes `.git`, which is
/// right for a `read_file` the model asked for and wrong here: denying it
/// would break `git status` and cargo's own vcs lookups. Only material that
/// is a credential is denied, because a sandbox that breaks the toolchain
/// gets turned off, and a sandbox that is off protects nothing.
const DENY_READ_PATTERNS: [&str; 8] = [
    r"/\.env",
    r"/\.ssh/",
    r"/\.aws/",
    r"/\.gnupg/",
    r"/\.netrc$",
    r"/\.git-credentials$",
    r"\.(pem|key|p12|pfx)$",
    r"/id_(rsa|dsa|ecdsa|ed25519)$",
];

/// Allowed back after the denials above, because an example dotenv carries no
/// value and is how the agent documents a variable it must not set.
const ALLOW_READ_PATTERNS: [&str; 1] = [r"/\.env\.(example|sample|template|dist)$"];

impl SandboxPolicy {
    /// The rules for a mode, given where the project is.
    pub fn new(mode: SandboxMode, root: &Path, allow_network: bool) -> Self {
        let writable_roots = match mode {
            SandboxMode::Off | SandboxMode::ReadOnly => Vec::new(),
            // A toolchain writes outside the project whatever we would
            // prefer: cargo and rustc use TMPDIR. Granting that one directory
            // is the narrowest thing that still lets `cargo test` run.
            //
            // Granting all of `/private/var/folders` instead would be far
            // wider than it looks: that is where every application's temp
            // space lives, not just ours. A test caught exactly that, by
            // escaping into a sibling temp directory.
            SandboxMode::WorkspaceWrite => {
                let mut roots = vec![root.to_path_buf()];
                let temp = std::env::temp_dir();
                roots.push(temp.canonicalize().unwrap_or(temp));
                roots
            }
        };
        Self {
            writable_roots,
            allow_network,
        }
    }
}

/// Pick a backend. `Off` asks for none; anything else needs the platform to
/// provide one, and says so when it cannot.
pub fn select(mode: SandboxMode) -> Backend {
    if mode == SandboxMode::Off {
        return Backend::None;
    }
    match cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists() {
        true => Backend::MacosSeatbelt,
        false => Backend::None,
    }
}

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The program and arguments that run `command` under `backend`.
///
/// Returned rather than executed so the caller keeps control of the child's
/// environment, process group and pipes, all of which matter as much as the
/// confinement does.
pub fn wrap(backend: Backend, policy: &SandboxPolicy, command: &str) -> (String, Vec<String>) {
    match backend {
        Backend::None => (
            "/bin/sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        ),
        Backend::MacosSeatbelt => (
            SANDBOX_EXEC.to_string(),
            vec![
                "-p".to_string(),
                seatbelt_policy(policy),
                "--".to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                command.to_string(),
            ],
        ),
    }
}

/// Render a Seatbelt policy.
///
/// Order matters: a later rule overrides an earlier one, which is how the
/// example dotenv is allowed back after the blanket denial of dotenv reads.
fn seatbelt_policy(policy: &SandboxPolicy) -> String {
    let mut rules = String::from("(version 1)\n(deny default)\n");

    // Reading is broadly allowed: a compiler needs the SDK, the toolchain and
    // half of /usr. Credentials are carved back out below, and whatever does
    // come back still passes through redaction before the model sees it.
    rules.push_str("(allow file-read*)\n");
    for pattern in DENY_READ_PATTERNS {
        rules.push_str(&format!("(deny file-read* (regex #\"{pattern}\"))\n"));
    }
    for pattern in ALLOW_READ_PATTERNS {
        rules.push_str(&format!("(allow file-read* (regex #\"{pattern}\"))\n"));
    }

    // Enough to run a toolchain: fork, exec, signal its own children, read
    // sysctls, reach the bootstrap server.
    rules.push_str(
        "(allow process-exec)\n\
         (allow process-fork)\n\
         (allow signal (target same-sandbox))\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow file-write-data (literal \"/dev/null\"))\n",
    );

    for root in &policy.writable_roots {
        rules.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            quote(&root.to_string_lossy())
        ));
    }

    // Remote network is how a read leaves the machine, so it stays denied
    // unless the user asked. Loopback is carved back in afterwards: a later
    // rule wins, and binding 127.0.0.1 does not leave the machine. Seatbelt's
    // `localhost` covers 127.0.0.1 and ::1.
    rules.push_str(match policy.allow_network {
        true => "(allow network*)\n",
        false => {
            "(deny network*)\n\
             (allow network-bind (local ip \"localhost:*\"))\n\
             (allow network-inbound (local ip \"localhost:*\"))\n\
             (allow network-outbound (remote ip \"localhost:*\"))\n"
        }
    });
    rules
}

/// Quote a path for SBPL. A path with a quote or backslash in it would
/// otherwise end the string early and change the policy's meaning.
fn quote(path: &str) -> String {
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: SandboxMode) -> SandboxPolicy {
        SandboxPolicy::new(mode, Path::new("/project"), false)
    }

    fn rendered(mode: SandboxMode) -> String {
        seatbelt_policy(&policy(mode))
    }

    #[test]
    fn off_asks_for_no_backend() {
        assert_eq!(select(SandboxMode::Off), Backend::None);
    }

    #[test]
    fn off_runs_the_command_exactly_as_before() {
        let (program, args) = wrap(Backend::None, &policy(SandboxMode::Off), "echo hi");
        assert_eq!(program, "/bin/sh");
        assert_eq!(args, vec!["-c", "echo hi"]);
    }

    #[test]
    fn seatbelt_wraps_the_shell_rather_than_replacing_it() {
        let (program, args) = wrap(
            Backend::MacosSeatbelt,
            &policy(SandboxMode::WorkspaceWrite),
            "echo hi",
        );
        assert_eq!(program, SANDBOX_EXEC);
        assert_eq!(args[0], "-p");
        assert_eq!(&args[2..], ["--", "/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn read_only_grants_no_writable_root() {
        assert!(policy(SandboxMode::ReadOnly).writable_roots.is_empty());
        assert!(!rendered(SandboxMode::ReadOnly).contains("file-write* (subpath"));
    }

    #[test]
    fn workspace_write_grants_the_project() {
        let rules = rendered(SandboxMode::WorkspaceWrite);
        assert!(
            rules.contains("(allow file-write* (subpath \"/project\"))"),
            "{rules}"
        );
    }

    #[test]
    fn workspace_write_grants_the_temp_dir_a_toolchain_needs() {
        // cargo and rustc write to TMPDIR; without this `cargo test` fails
        // inside the sandbox and the sandbox gets turned off.
        let temp = std::env::temp_dir();
        let temp = temp.canonicalize().unwrap_or(temp);
        let rules = rendered(SandboxMode::WorkspaceWrite);
        assert!(
            rules.contains(&temp.to_string_lossy().to_string()),
            "{rules}"
        );
    }

    #[test]
    fn the_temp_grant_is_one_directory_not_everyones() {
        // `/private/var/folders` holds every application's temp space. A test
        // escaped into a sibling directory when that whole tree was granted.
        let rules = rendered(SandboxMode::WorkspaceWrite);
        assert!(
            !rules.contains("(subpath \"/private/var/folders\")"),
            "the whole temp tree is writable: {rules}"
        );
    }

    #[test]
    fn network_is_denied_unless_asked_for() {
        let rules = rendered(SandboxMode::WorkspaceWrite);
        assert!(rules.contains("(deny network*)"));
        assert!(!rules.contains("(allow network*)\n"));

        let allowed = SandboxPolicy::new(SandboxMode::WorkspaceWrite, Path::new("/project"), true);
        let open = seatbelt_policy(&allowed);
        assert!(open.contains("(allow network*)"));
        assert!(!open.contains("(deny network*)"));
    }

    #[test]
    fn loopback_stays_open_when_the_remote_network_is_denied() {
        let rules = rendered(SandboxMode::ReadOnly);
        let denied = rules
            .find("(deny network*)")
            .expect("remote network denied");
        for allow in [
            "(allow network-bind (local ip \"localhost:*\"))",
            "(allow network-inbound (local ip \"localhost:*\"))",
            "(allow network-outbound (remote ip \"localhost:*\"))",
        ] {
            let at = rules
                .find(allow)
                .unwrap_or_else(|| panic!("missing {allow} in {rules}"));
            assert!(at > denied, "{allow} must follow the denial so it wins");
        }
    }

    #[test]
    fn credentials_are_denied_and_the_example_dotenv_is_not() {
        let rules = rendered(SandboxMode::WorkspaceWrite);
        assert!(
            rules.contains(r#"(deny file-read* (regex #"/\.env"))"#),
            "{rules}"
        );
        assert!(rules.contains(r"/\.ssh/"), "{rules}");
        assert!(rules.contains(r"\.(pem|key|p12|pfx)$"), "{rules}");

        // Order is the mechanism: the allow has to come after the deny.
        let denied = rules.find(r#"(deny file-read* (regex #"/\.env"))"#);
        let allowed = rules.find(r"\.env\.(example|sample|template|dist)$");
        assert!(allowed > denied, "the allow-back must follow the denial");
    }

    #[test]
    fn the_git_directory_stays_readable() {
        // `policy.rs` denies it to `read_file`; denying it here would break
        // `git status` and cargo's vcs lookup, and a broken sandbox is one
        // the user switches off.
        assert!(!rendered(SandboxMode::WorkspaceWrite).contains(r"/\.git/"));
    }

    #[test]
    fn a_path_cannot_break_out_of_its_quotes() {
        let awkward = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            Path::new(r#"/tmp/a"b\c"#),
            false,
        );
        let rules = seatbelt_policy(&awkward);
        assert!(rules.contains(r#""/tmp/a\"b\\c""#), "{rules}");
    }
}
