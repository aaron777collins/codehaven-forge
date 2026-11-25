//! Workflow YAML model, validation, the `${{ }}` expression engine, matrix expansion and
//! trigger matching. Pure: no IO and no async. The server and the runner both use it, so a
//! step's `if:` means the same thing in both.
//!
//! - [`parse::parse`] reads a file and reports every problem with its line.
//! - [`expr`] evaluates `${{ }}` expressions and `if:` conditions.
//! - [`plan::plan`] expands matrices and names the jobs of a run.
//! - [`trigger`] decides which events start a workflow.
//! - [`options`] is the allow list for `container.options`, used by the server and the runner.
//! - [`actions`] names the built-in actions a `uses:` may use.
//!
//! See ARCHITECTURE.md section 9.

pub mod action_def;
pub mod actions;
pub mod diag;
pub mod discover;
pub mod expr;
pub mod glob;
pub mod inputs;
pub mod model;
pub mod options;
pub mod parse;
pub mod permissions;
pub mod plan;
pub mod refs;
pub mod schedule;
pub mod trigger;
pub mod uses;

pub use diag::Diagnostic;
pub use model::*;
pub use parse::parse;

/// Where the workflow files live in a repository, in order of precedence.
pub const WORKFLOW_DIRS: &[&str] = &[".codehaven/workflows", ".github/workflows"];

/// True for `.yml` and `.yaml` file names.
pub fn is_workflow_file(name: &str) -> bool {
    name.ends_with(".yml") || name.ends_with(".yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_workflow_files() {
        assert!(is_workflow_file("ci.yml"));
        assert!(is_workflow_file("release.yaml"));
        assert!(!is_workflow_file("README.md"));
    }
}
