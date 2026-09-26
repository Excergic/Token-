//! Committed transcript cells. The active tool call is not one of these until
//! it finishes; the widget holds that separately so it can change in place.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        arguments: String,
        output: String,
        failed: bool,
    },
    Error(String),
}

impl Cell {
    pub fn tool(
        name: impl Into<String>,
        arguments: impl Into<String>,
        output: impl Into<String>,
        failed: bool,
    ) -> Self {
        Self::Tool {
            name: name.into(),
            arguments: arguments.into(),
            output: output.into(),
            failed,
        }
    }
}
