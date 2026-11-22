//! Validation messages that name the line.

use std::fmt;

/// One problem found in a workflow file. Lines and columns start at 1. Line 0 means the
/// problem is not tied to one place, such as a missing `jobs:` key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    pub line: u32,
    pub column: u32,
    pub message: String,
}

impl Diagnostic {
    pub fn new(line: u32, column: u32, message: impl Into<String>) -> Self {
        Self { line, column, message: message.into() }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 { f.write_str(&self.message) } else { write!(f, "line {}: {}", self.line, self.message) }
    }
}

/// Renders every diagnostic, one per line, for logs and API messages.
pub fn render(diags: &[Diagnostic]) -> String {
    diags.iter().map(Diagnostic::to_string).collect::<Vec<_>>().join("\n")
}
