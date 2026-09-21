//! Provider-agnostic conversation types shared by the runtime and the LLM client.
//!
//! The runtime owns a `Conversation` and decides what goes into it. The LLM
//! client only reads it and converts to the provider's wire format privately.

/// One turn in the conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    /// Result of running a tool. Nothing produces this until tools exist.
    #[allow(dead_code)]
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// A request from the model to run a tool. Arguments are kept as the raw JSON
/// string the model produced; whoever runs the tool parses them.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// The transcript so far, in order.
#[derive(Debug, Default)]
pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
}
