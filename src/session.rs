//! Session history: the transcript, on disk, so a later run can continue it.
//!
//! One SQLite file holds every session; a session is keyed by the directory
//! the agent ran in, so `--resume` in a project picks up that project's last
//! conversation without anyone naming an id. The `id` column is already there
//! for named sessions later; cwd is just the only way to find a row today.
//!
//! Writes are write-through: the runtime records each message as it appends it
//! rather than flushing a list at exit, so there is no exit path to miss and a
//! run killed mid-tool-loop loses nothing.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::conversation::{Message, ToolCall};
use crate::secrets;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("could not create `{path}`: {source}")]
    Create {
        path: String,
        source: std::io::Error,
    },
    #[error("stored message {id} has an unknown role `{role}`")]
    UnknownRole { id: i64, role: String },
    #[error("could not decode stored tool calls for message {id}: {source}")]
    BadToolCalls { id: i64, source: serde_json::Error },
}

/// An open session. Holds the row id; the store behind it does the writing.
pub struct Session<'a> {
    store: &'a SessionStore,
    id: i64,
}

impl Session<'_> {
    /// Record one message at its position in the transcript. Called by the
    /// runtime before the message goes into the `Conversation`, so what is on
    /// disk is never behind what is in memory.
    /// Redacts on the way in. Tool results are already clean by the time they
    /// arrive, but a task the user typed, or an answer the model composed, can
    /// carry a secret too, and this file outlives the run that wrote it.
    pub fn append(&self, seq: usize, message: &Message) -> Result<(), SessionError> {
        let message = &redacted(message);
        let (role, content, tool_calls, tool_call_id) = match message {
            Message::System(content) => ("system", Some(content.as_str()), None, None),
            Message::User(content) => ("user", Some(content.as_str()), None, None),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let encoded = (!tool_calls.is_empty()).then(|| {
                    let stored: Vec<StoredToolCall> =
                        tool_calls.iter().map(StoredToolCall::from).collect();
                    serde_json::to_string(&stored).expect("tool calls are plain strings")
                });
                ("assistant", content.as_deref(), encoded, None)
            }
            Message::Tool {
                tool_call_id,
                content,
            } => (
                "tool",
                Some(content.as_str()),
                None,
                Some(tool_call_id.as_str()),
            ),
        };

        self.store.conn.execute(
            "INSERT INTO messages (session_id, seq, role, content, tool_calls, tool_call_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            params![self.id, seq as i64, role, content, tool_calls, tool_call_id],
        )?;
        self.store.conn.execute(
            "UPDATE sessions SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![self.id],
        )?;
        Ok(())
    }
}

/// A copy of `message` with any credential in it rewritten.
fn redacted(message: &Message) -> Message {
    match message {
        Message::System(content) => Message::System(secrets::redact(content).text),
        Message::User(content) => Message::User(secrets::redact(content).text),
        Message::Assistant {
            content,
            tool_calls,
        } => Message::Assistant {
            content: content
                .as_deref()
                .map(|content| secrets::redact(content).text),
            tool_calls: tool_calls
                .iter()
                .map(|call| ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    // The model puts file contents in here when it writes one.
                    arguments: secrets::redact(&call.arguments).text,
                })
                .collect(),
        },
        Message::Tool {
            tool_call_id,
            content,
        } => Message::Tool {
            tool_call_id: tool_call_id.clone(),
            content: secrets::redact(content).text,
        },
    }
}

/// A resumed session and the transcript to replay into it.
pub struct Resumed<'a> {
    pub session: Session<'a>,
    pub messages: Vec<Message>,
}

pub struct SessionStore {
    conn: Connection,
}

impl SessionStore {
    /// Open (creating if needed) the session database.
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SessionError::Create {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        // WAL so a reader is never blocked by the run that is writing.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Begin a new session for this directory.
    pub fn start(&self, root: &str, model: &str) -> Result<Session<'_>, SessionError> {
        self.conn.execute(
            "INSERT INTO sessions (root, model, created_at, updated_at)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                         strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            params![root, model],
        )?;
        Ok(Session {
            store: self,
            id: self.conn.last_insert_rowid(),
        })
    }

    /// The most recent session for this directory, with its transcript ready
    /// to replay. `None` means nothing has run here yet.
    ///
    /// The transcript is repaired before it is returned, and the repair is
    /// written back, so what is on disk stays equal to what is replayed.
    pub fn resume(&self, root: &str) -> Result<Option<Resumed<'_>>, SessionError> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE root = ?1 ORDER BY updated_at DESC, id DESC LIMIT 1",
                params![root],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Ok(None);
        };

        let messages = self.messages(id)?;
        let keep = repaired_len(&messages);
        if keep < messages.len() {
            self.conn.execute(
                "DELETE FROM messages WHERE session_id = ?1 AND seq >= ?2",
                params![id, keep as i64],
            )?;
        }

        Ok(Some(Resumed {
            session: Session { store: self, id },
            messages: messages.into_iter().take(keep).collect(),
        }))
    }

    fn messages(&self, session_id: i64) -> Result<Vec<Message>, SessionError> {
        let mut statement = self.conn.prepare(
            "SELECT id, role, content, tool_calls, tool_call_id
             FROM messages WHERE session_id = ?1 ORDER BY seq",
        )?;
        let rows = statement.query_map(params![session_id], |row| {
            Ok(StoredMessage {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                tool_calls: row.get(3)?,
                tool_call_id: row.get(4)?,
            })
        })?;

        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?.into_message()?);
        }
        Ok(messages)
    }
}

/// How much of a stored transcript is safe to replay.
///
/// A run killed mid-loop leaves an assistant turn whose tool calls were never
/// answered. Both wire formats reject that: chat completions wants every
/// `tool_call_id` answered, the Responses API wants a `function_call_output`
/// for every `function_call`. So the tail is cut back to the last point where
/// every call has its result.
fn repaired_len(messages: &[Message]) -> usize {
    let mut safe = messages.len();
    for (index, message) in messages.iter().enumerate() {
        let Message::Assistant { tool_calls, .. } = message else {
            continue;
        };
        if tool_calls.is_empty() {
            continue;
        }
        let answered: Vec<&str> = messages[index + 1..]
            .iter()
            .map_while(|message| match message {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        if !tool_calls
            .iter()
            .all(|call| answered.contains(&call.id.as_str()))
        {
            safe = index;
            break;
        }
    }
    safe
}

struct StoredMessage {
    id: i64,
    role: String,
    content: Option<String>,
    tool_calls: Option<String>,
    tool_call_id: Option<String>,
}

impl StoredMessage {
    fn into_message(self) -> Result<Message, SessionError> {
        Ok(match self.role.as_str() {
            "system" => Message::System(self.content.unwrap_or_default()),
            "user" => Message::User(self.content.unwrap_or_default()),
            "assistant" => Message::Assistant {
                content: self.content,
                tool_calls: match self.tool_calls {
                    Some(encoded) => serde_json::from_str::<Vec<StoredToolCall>>(&encoded)
                        .map_err(|source| SessionError::BadToolCalls {
                            id: self.id,
                            source,
                        })?
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    None => Vec::new(),
                },
            },
            "tool" => Message::Tool {
                tool_call_id: self.tool_call_id.unwrap_or_default(),
                content: self.content.unwrap_or_default(),
            },
            role => {
                return Err(SessionError::UnknownRole {
                    id: self.id,
                    role: role.to_string(),
                });
            }
        })
    }
}

/// Storage shape for a tool call. Declared here rather than deriving serde on
/// `ToolCall`, so `conversation.rs` stays plain types with no storage concern.
#[derive(Serialize, Deserialize)]
struct StoredToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl From<&ToolCall> for StoredToolCall {
    fn from(call: &ToolCall) -> Self {
        Self {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        }
    }
}

impl From<StoredToolCall> for ToolCall {
    fn from(call: StoredToolCall) -> Self {
        Self {
            id: call.id,
            name: call.name,
            arguments: call.arguments,
        }
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id         INTEGER PRIMARY KEY,
    root       TEXT NOT NULL,
    model      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_by_root ON sessions (root, updated_at DESC);

CREATE TABLE IF NOT EXISTS messages (
    id           INTEGER PRIMARY KEY,
    session_id   INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,
    role         TEXT NOT NULL,
    content      TEXT,
    tool_calls   TEXT,
    tool_call_id TEXT,
    created_at   TEXT NOT NULL,
    UNIQUE (session_id, seq)
);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SessionStore {
        // In-memory, so a test never touches the real database.
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        SessionStore { conn }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: r#"{"path":"src/main.rs"}"#.into(),
        }
    }

    fn write(store: &SessionStore, root: &str, messages: &[Message]) {
        let session = store.start(root, "gpt-5.5").unwrap();
        for (seq, message) in messages.iter().enumerate() {
            session.append(seq, message).unwrap();
        }
    }

    #[test]
    fn nothing_to_resume_in_a_fresh_directory() {
        let store = store();
        assert!(store.resume("/tmp/nowhere").unwrap().is_none());
    }

    #[test]
    fn round_trips_every_message_variant() {
        let store = store();
        let messages = vec![
            Message::System("be brief".into()),
            Message::User("hi".into()),
            Message::Assistant {
                content: Some("reading".into()),
                tool_calls: vec![call("call-1")],
            },
            Message::Tool {
                tool_call_id: "call-1".into(),
                content: "fn main() {}".into(),
            },
            Message::Assistant {
                content: Some("done".into()),
                tool_calls: vec![],
            },
        ];
        write(&store, "/p", &messages);

        let resumed = store.resume("/p").unwrap().unwrap();
        assert_eq!(resumed.messages, messages);
    }

    #[test]
    fn the_stored_system_prompt_comes_back_byte_for_byte() {
        let store = store();
        let prompt = "You are token. The only tools that exist are: read_file.";
        write(&store, "/p", &[Message::System(prompt.into())]);

        let resumed = store.resume("/p").unwrap().unwrap();
        assert_eq!(resumed.messages[0], Message::System(prompt.into()));
    }

    #[test]
    fn resumes_the_most_recent_session_for_a_directory() {
        let store = store();
        write(&store, "/p", &[Message::User("first".into())]);
        write(&store, "/p", &[Message::User("second".into())]);

        let resumed = store.resume("/p").unwrap().unwrap();
        assert_eq!(resumed.messages, vec![Message::User("second".into())]);
    }

    #[test]
    fn directories_do_not_see_each_others_history() {
        let store = store();
        write(&store, "/a", &[Message::User("from a".into())]);
        write(&store, "/b", &[Message::User("from b".into())]);

        assert_eq!(
            store.resume("/a").unwrap().unwrap().messages,
            vec![Message::User("from a".into())]
        );
    }

    #[test]
    fn drops_a_tool_call_that_was_never_answered() {
        // What a run killed mid-loop leaves behind. Replaying it is a 400.
        let store = store();
        write(
            &store,
            "/p",
            &[
                Message::User("read it".into()),
                Message::Assistant {
                    content: None,
                    tool_calls: vec![call("call-1")],
                },
            ],
        );

        let resumed = store.resume("/p").unwrap().unwrap();
        assert_eq!(resumed.messages, vec![Message::User("read it".into())]);
    }

    #[test]
    fn drops_a_turn_where_only_some_calls_were_answered() {
        let store = store();
        write(
            &store,
            "/p",
            &[
                Message::User("read both".into()),
                Message::Assistant {
                    content: None,
                    tool_calls: vec![call("call-1"), call("call-2")],
                },
                Message::Tool {
                    tool_call_id: "call-1".into(),
                    content: "fn main() {}".into(),
                },
            ],
        );

        let resumed = store.resume("/p").unwrap().unwrap();
        assert_eq!(resumed.messages, vec![Message::User("read both".into())]);
    }

    #[test]
    fn the_repair_is_written_back_so_disk_matches_what_is_replayed() {
        let store = store();
        write(
            &store,
            "/p",
            &[
                Message::User("read it".into()),
                Message::Assistant {
                    content: None,
                    tool_calls: vec![call("call-1")],
                },
            ],
        );

        let first = store.resume("/p").unwrap().unwrap().messages.len();
        // Appending after a repair must not collide with a deleted seq.
        let session = store.resume("/p").unwrap().unwrap().session;
        session
            .append(first, &Message::User("again".into()))
            .unwrap();

        let resumed = store.resume("/p").unwrap().unwrap();
        assert_eq!(
            resumed.messages,
            vec![
                Message::User("read it".into()),
                Message::User("again".into())
            ]
        );
    }

    #[test]
    fn a_secret_never_reaches_the_database() {
        // The database outlives the run. A task the user typed is the one
        // path redaction at the tool boundary does not cover.
        let store = store();
        write(
            &store,
            "/p",
            &[
                Message::User("deploy with AKIAIOSFODNN7EXAMPLE".into()),
                Message::Assistant {
                    content: Some("using sk-abcdefghijklmnop1234".into()),
                    tool_calls: vec![],
                },
            ],
        );

        let resumed = store.resume("/p").unwrap().unwrap();
        let stored = format!("{:?}", resumed.messages);
        assert!(!stored.contains("AKIAIOSFODNN7EXAMPLE"), "{stored}");
        assert!(!stored.contains("sk-abcdefghijklmnop"), "{stored}");
        assert!(stored.contains("REDACTED"), "{stored}");
    }

    #[test]
    fn a_secret_in_a_write_tool_argument_is_redacted_too() {
        // write_file carries the file's whole content in its arguments.
        let store = store();
        write(
            &store,
            "/p",
            &[Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "write_file".into(),
                    arguments: r#"{"path":"c.json","content":"token=abc123def456"}"#.into(),
                }],
            }],
        );

        let stored = format!("{:?}", store.resume("/p").unwrap().unwrap().messages);
        assert!(!stored.contains("abc123def456"), "{stored}");
    }

    #[test]
    fn keeps_a_completed_tool_loop() {
        let store = store();
        let messages = vec![
            Message::User("read it".into()),
            Message::Assistant {
                content: None,
                tool_calls: vec![call("call-1")],
            },
            Message::Tool {
                tool_call_id: "call-1".into(),
                content: "fn main() {}".into(),
            },
            Message::Assistant {
                content: Some("it is a main function".into()),
                tool_calls: vec![],
            },
        ];
        write(&store, "/p", &messages);

        assert_eq!(store.resume("/p").unwrap().unwrap().messages, messages);
    }
}
