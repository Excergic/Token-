//! Redaction: the backstop for secrets that `policy.rs` never had a chance to
//! refuse.
//!
//! `policy.rs` denies files it can name. It cannot name a credential sitting
//! inside `config/settings.json`, a log, or a compose file. Once such a value
//! is read it goes to the provider on every later turn and into the session
//! database permanently, so it is rewritten at two boundaries: every tool's
//! output before the model sees it, and everything on its way to disk.
//!
//! Patterns are recognised by shape, and the one generic rule (`key = value`)
//! is deliberately narrow. This module reads source code all day; turning
//! `let token = self.next_token()` into `[REDACTED_SECRET]` would corrupt the
//! files it is meant to be reading. A missed secret is a risk, but a corrupted
//! file is a certainty, so the generic rule demands a value that looks like a
//! credential and nothing like an expression.

use regex::Regex;
use std::sync::LazyLock;

pub const MARKER: &str = "[REDACTED_SECRET]";

/// Credentials with a distinctive shape. Matched whole and replaced whole.
static SHAPED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // OpenAI and Stripe style, including sk-proj- and sk_live_ forms.
        r"\bsk[-_][A-Za-z0-9_-]{12,}",
        // AWS access key id.
        r"\bAKIA[0-9A-Z]{16}\b",
        // GitHub tokens.
        r"\bgh[pousr]_[A-Za-z0-9]{20,}",
        r"\bgithub_pat_[A-Za-z0-9_]{20,}",
        // Slack.
        r"\bxox[baprs]-[A-Za-z0-9-]{10,}",
        // Google API keys.
        r"\bAIza[A-Za-z0-9_-]{30,}",
        // A bearer token in a header or a curl line.
        r"(?i)\bBearer\s+[A-Za-z0-9._~+/-]{20,}=*",
        // Any PEM private key block, however long.
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("static pattern compiles"))
    .collect()
});

/// `SOME_KEY = "value"`. Only the value is replaced: the name is what tells
/// the model, and the user, what was removed.
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)([A-Za-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|access[_-]?key|credential)[A-Za-z0-9_.-]*["']?\s*[:=]\s*)(["']?)([A-Za-z0-9_\-+/=.~]{8,})(["']?)"#,
    )
    .expect("static pattern compiles")
});

/// Text with its secrets replaced, and how many were found.
pub struct Redacted {
    pub text: String,
    pub count: usize,
}

/// Rewrite anything that looks like a credential.
pub fn redact(input: &str) -> Redacted {
    let mut count = 0;
    let mut text = input.to_string();

    for pattern in SHAPED.iter() {
        text = pattern
            .replace_all(&text, |_: &regex::Captures| {
                count += 1;
                MARKER
            })
            .into_owned();
    }

    text = ASSIGNMENT
        .replace_all(&text, |captures: &regex::Captures| {
            let value = &captures[3];
            if !looks_like_a_credential(value) {
                return captures[0].to_string();
            }
            count += 1;
            format!("{}{}{}{}", &captures[1], &captures[2], MARKER, &captures[4])
        })
        .into_owned();

    Redacted { text, count }
}

/// The guard on the generic rule. A credential mixes letters and digits and
/// is not a word; an identifier, a number, a version or a boolean is not one.
fn looks_like_a_credential(value: &str) -> bool {
    if value == MARKER || value.contains(MARKER) {
        return false;
    }
    let has_letter = value.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    has_letter && has_digit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redacted(input: &str) -> String {
        redact(input).text
    }

    fn untouched(input: &str) {
        let result = redact(input);
        assert_eq!(result.text, input, "wrongly redacted");
        assert_eq!(result.count, 0);
    }

    #[test]
    fn redacts_provider_keys() {
        assert_eq!(redacted("sk-proj-abcdefghijklmnop1234"), MARKER);
        assert_eq!(redacted("sk_live_abc123def456ghi"), MARKER);
        assert!(!redacted("key is sk-abcdefghijklmnopqrst here").contains("sk-abcdef"));
    }

    #[test]
    fn redacts_cloud_and_platform_tokens() {
        assert_eq!(redacted("AKIAIOSFODNN7EXAMPLE"), MARKER);
        assert_eq!(redacted("ghp_abcdefghijklmnopqrstuvwxyz0123456789"), MARKER);
        assert_eq!(redacted("xoxb-1234567890-abcdefghij"), MARKER);
    }

    #[test]
    fn redacts_a_bearer_header() {
        let curl = "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9'";
        assert!(!redacted(curl).contains("eyJhbGci"));
    }

    #[test]
    fn redacts_a_whole_private_key_block() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKC\nAQEA\n-----END RSA PRIVATE KEY-----";
        assert_eq!(redacted(pem), MARKER);
    }

    #[test]
    fn redacts_the_value_but_keeps_the_name() {
        let out = redacted(r#"STRIPE_API_KEY="pk7fj39dkw02ncba""#);
        assert!(out.starts_with("STRIPE_API_KEY="), "name lost: {out}");
        assert!(out.contains(MARKER), "value kept: {out}");
        assert!(!out.contains("pk7fj39dkw02ncba"));
    }

    #[test]
    fn redacts_a_secret_in_a_config_file_policy_cannot_name() {
        // The exact gap this module exists for: not .env, so never refused.
        let json = r#"{ "database_password": "hunter2000abc", "port": 5432 }"#;
        let out = redacted(json);
        assert!(!out.contains("hunter2000abc"), "{out}");
        assert!(out.contains("5432"), "unrelated value lost: {out}");
    }

    // --- what must survive, because this agent reads source code ---

    #[test]
    fn leaves_ordinary_rust_alone() {
        untouched("let token = self.next_token();");
        untouched("fn parse_token(&mut self) -> Token { self.token.clone() }");
        untouched("const MAX_TOKENS: usize = 4096;");
        untouched("if secret.is_empty() { return Err(e); }");
    }

    #[test]
    fn leaves_numbers_and_identifiers_alone() {
        // No letter, or no digit: not a credential either way.
        untouched("max_tokens = 128000");
        untouched("api_key = None");
        untouched("password = prompt_for_password");
    }

    #[test]
    fn leaves_placeholders_alone_enough_to_stay_useful() {
        // The example dotenv we tell the model to write must survive intact.
        untouched("OPENAI_API_KEY=your_key_here");
        untouched("SARVAM_API_KEY=your_key_here");
    }

    #[test]
    fn leaves_prose_about_secrets_alone() {
        untouched("Store the API key in .env and never commit it.");
        untouched("The token is read from the environment.");
    }

    #[test]
    fn counts_what_it_changed() {
        let result = redact("a sk-abcdefghijklmnop1 b AKIAIOSFODNN7EXAMPLE c");
        assert_eq!(result.count, 2);
        assert!(!result.text.contains("AKIA"));
    }

    #[test]
    fn redacting_twice_changes_nothing_further() {
        // A resumed transcript is redacted text going back through the same
        // boundary; it must not become [REDACTED_[REDACTED_SECRET]].
        let once = redact(r#"API_KEY="sk-abcdefghijklmnop1234""#).text;
        let twice = redact(&once);
        assert_eq!(twice.text, once);
        assert_eq!(twice.count, 0);
    }

    #[test]
    fn nothing_to_do_is_free() {
        untouched("");
        untouched("fn main() {}");
    }
}
