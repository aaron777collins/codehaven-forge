//! The command line, as clap sees it. One enum per command group, one function per group in
//! `cmd/` that runs it.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "haven", version, about = "Codehaven command line client", propagate_version = true)]
pub struct Cli {
    /// The Codehaven host, such as `code.example.com`. Defaults to `HAVEN_HOST`, then the host
    /// you last signed in to.
    #[arg(long, global = true, env = "HAVEN_HOST")]
    pub host: Option<String>,

    /// The repository, as `owner/name`. Defaults to the `origin` remote of the current directory.
    #[arg(short = 'R', long, global = true)]
    pub repo: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in, sign out and check your sign-in.
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Create, clone, view, fork and list repositories.
    #[command(subcommand)]
    Repo(RepoCmd),
    /// List, view, create, close and comment on issues.
    #[command(subcommand)]
    Issue(IssueCmd),
    /// Work with pull requests.
    #[command(subcommand)]
    Pr(PrCmd),
    /// Watch and manage workflow runs.
    #[command(subcommand)]
    Run(RunCmd),
    /// List and start workflows.
    #[command(subcommand)]
    Workflow(WorkflowCmd),
    /// Create, list and upload releases.
    #[command(subcommand)]
    Release(ReleaseCmd),
    /// Make a raw REST call and print the answer.
    Api(ApiArgs),
}

// ---- auth ----

#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    /// Store a personal access token for a host.
    Login(LoginArgs),
    /// Forget the stored token for a host.
    Logout,
    /// Show who you are signed in as.
    Status,
    /// Print the stored token (for scripts).
    Token,
    /// The git credential helper: `git config credential.helper '!haven auth git-credential'`.
    GitCredential {
        /// `get`, `store` or `erase`, as git passes it.
        operation: String,
    },
    /// Point git at `haven` for this host's credentials.
    SetupGit,
}

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Read the token from standard input instead of asking for it.
    #[arg(long)]
    pub with_token: bool,
}

// ---- repo ----

#[derive(Subcommand, Debug)]
pub enum RepoCmd {
    /// Create a repository.
    Create(RepoCreateArgs),
    /// Clone a repository.
    Clone(RepoCloneArgs),
    /// Show a repository.
    View(RepoViewArgs),
    /// Fork a repository into your account or an organization.
    Fork(RepoForkArgs),
    /// List repositories of a user or organization (yours by default).
    List(RepoListArgs),
}

#[derive(Args, Debug)]
pub struct RepoCreateArgs {
    /// `name`, or `owner/name` to create it under an organization.
    pub name: String,
    #[arg(long, short)]
    pub description: Option<String>,
    /// Let every signed-in account on this instance read it. Repositories are private by
    /// default. Nothing here is readable without signing in.
    #[arg(long)]
    pub public: bool,
    /// Create a first commit with an empty README.
    #[arg(long)]
    pub add_readme: bool,
    /// Clone it afterwards.
    #[arg(long)]
    pub clone: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct RepoCloneArgs {
    /// `owner/name`
    pub repo: String,
    /// Where to clone to.
    pub directory: Option<String>,
    /// Extra arguments for `git clone`, after `--`.
    #[arg(last = true)]
    pub git_args: Vec<String>,
}

#[derive(Args, Debug)]
pub struct RepoViewArgs {
    /// `owner/name`. Defaults to the current directory's repository.
    pub repo: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct RepoForkArgs {
    /// `owner/name`. Defaults to the current directory's repository.
    pub repo: Option<String>,
    /// Fork into this organization instead of your account.
    #[arg(long)]
    pub org: Option<String>,
    /// Name for the fork.
    #[arg(long)]
    pub name: Option<String>,
    /// Clone the fork afterwards.
    #[arg(long)]
    pub clone: bool,
}

#[derive(Args, Debug)]
pub struct RepoListArgs {
    /// A user or organization. Defaults to you.
    pub owner: Option<String>,
    #[arg(long, short = 'L', default_value_t = 30)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

// ---- issue ----

#[derive(Subcommand, Debug)]
pub enum IssueCmd {
    List(IssueListArgs),
    View(IssueViewArgs),
    Create(IssueCreateArgs),
    Close(IssueCloseArgs),
    Comment(IssueCommentArgs),
}

#[derive(Args, Debug)]
pub struct IssueListArgs {
    /// `open`, `closed` or `all`.
    #[arg(long, short, default_value = "open")]
    pub state: String,
    #[arg(long, short)]
    pub label: Vec<String>,
    #[arg(long, short)]
    pub assignee: Option<String>,
    #[arg(long, short = 'L', default_value_t = 30)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct IssueViewArgs {
    pub number: u64,
    /// Also print the comments.
    #[arg(long, short)]
    pub comments: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct IssueCreateArgs {
    #[arg(long, short)]
    pub title: String,
    #[arg(long, short, default_value = "")]
    pub body: String,
    /// Read the body from a file (`-` for standard input).
    #[arg(long, short = 'F', conflicts_with = "body")]
    pub body_file: Option<String>,
    #[arg(long, short)]
    pub label: Vec<String>,
    #[arg(long, short)]
    pub assignee: Vec<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct IssueCloseArgs {
    pub number: u64,
    /// Leave a comment when closing.
    #[arg(long, short)]
    pub comment: Option<String>,
    /// `completed` or `not_planned`.
    #[arg(long, short, default_value = "completed")]
    pub reason: String,
}

#[derive(Args, Debug)]
pub struct IssueCommentArgs {
    pub number: u64,
    #[arg(long, short)]
    pub body: Option<String>,
    /// Read the comment from a file (`-` for standard input).
    #[arg(long, short = 'F', conflicts_with = "body")]
    pub body_file: Option<String>,
}

// ---- pr ----

#[derive(Subcommand, Debug)]
pub enum PrCmd {
    Create(PrCreateArgs),
    List(PrListArgs),
    View(PrViewArgs),
    /// Check out a pull request's branch.
    Checkout(PrCheckoutArgs),
    /// Print a pull request's diff.
    Diff(PrNumberArgs),
    /// Approve, request changes or comment.
    Review(PrReviewArgs),
    Merge(PrMergeArgs),
}

#[derive(Args, Debug)]
pub struct PrCreateArgs {
    #[arg(long, short)]
    pub title: String,
    #[arg(long, short, default_value = "")]
    pub body: String,
    #[arg(long, short = 'F', conflicts_with = "body")]
    pub body_file: Option<String>,
    /// The branch to merge into. Defaults to the repository's default branch.
    #[arg(long, short = 'B')]
    pub base: Option<String>,
    /// The branch with your changes, or `owner:branch` for a fork. Defaults to the current branch.
    #[arg(long, short = 'H')]
    pub head: Option<String>,
    #[arg(long, short)]
    pub draft: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct PrListArgs {
    /// `open`, `closed`, `merged` or `all`.
    #[arg(long, short, default_value = "open")]
    pub state: String,
    #[arg(long, short = 'B')]
    pub base: Option<String>,
    #[arg(long, short = 'H')]
    pub head: Option<String>,
    #[arg(long, short = 'L', default_value_t = 30)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct PrViewArgs {
    pub number: u64,
    /// Also print the reviews and the checks.
    #[arg(long, short)]
    pub comments: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct PrNumberArgs {
    pub number: u64,
}

#[derive(Args, Debug)]
pub struct PrCheckoutArgs {
    pub number: u64,
    /// Local branch name. Defaults to the pull request's branch name.
    #[arg(long, short)]
    pub branch: Option<String>,
}

#[derive(Args, Debug)]
pub struct PrReviewArgs {
    pub number: u64,
    #[arg(long, short, group = "verdict")]
    pub approve: bool,
    #[arg(long, short, group = "verdict")]
    pub request_changes: bool,
    #[arg(long, short, group = "verdict")]
    pub comment: bool,
    #[arg(long, short)]
    pub body: Option<String>,
}

#[derive(Args, Debug)]
pub struct PrMergeArgs {
    pub number: u64,
    #[arg(long, group = "method")]
    pub squash: bool,
    #[arg(long, group = "method")]
    pub rebase: bool,
    #[arg(long, group = "method")]
    pub merge: bool,
    /// Title for the merge or squash commit.
    #[arg(long)]
    pub subject: Option<String>,
    /// Body for the merge or squash commit.
    #[arg(long)]
    pub body: Option<String>,
}

// ---- run / workflow ----

#[derive(Subcommand, Debug)]
pub enum RunCmd {
    List(RunListArgs),
    /// Show a run and its jobs.
    View(RunIdArgs),
    /// Follow a run until it finishes. Exits non-zero unless it succeeded.
    Watch(RunWatchArgs),
    /// Print the logs of a run's jobs.
    Logs(RunLogsArgs),
    Rerun(RunRerunArgs),
    Cancel(RunIdArgs),
}

#[derive(Args, Debug)]
pub struct RunListArgs {
    /// A workflow's id or file name.
    #[arg(long, short)]
    pub workflow: Option<String>,
    #[arg(long, short)]
    pub branch: Option<String>,
    /// `queued`, `in_progress`, `completed`, or a conclusion such as `success` or `failure`.
    #[arg(long, short)]
    pub status: Option<String>,
    #[arg(long, short = 'L', default_value_t = 20)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct RunIdArgs {
    pub run_id: i64,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct RunWatchArgs {
    pub run_id: i64,
    /// Seconds between checks.
    #[arg(long, default_value_t = 3)]
    pub interval: u64,
}

#[derive(Args, Debug)]
pub struct RunLogsArgs {
    pub run_id: i64,
    /// Only this job (its id, or part of its name).
    #[arg(long, short)]
    pub job: Option<String>,
}

#[derive(Args, Debug)]
pub struct RunRerunArgs {
    pub run_id: i64,
    /// Only the jobs that failed.
    #[arg(long)]
    pub failed: bool,
}

#[derive(Subcommand, Debug)]
pub enum WorkflowCmd {
    List(WorkflowListArgs),
    /// Start a workflow that has a `workflow_dispatch` trigger.
    Run(WorkflowRunArgs),
}

#[derive(Args, Debug)]
pub struct WorkflowListArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct WorkflowRunArgs {
    /// The workflow's id or file name, such as `ci.yml`.
    pub workflow: String,
    /// The branch or tag to run on. Defaults to the default branch.
    #[arg(long, short = 'r')]
    pub r#ref: Option<String>,
    /// An input, as `name=value`. Repeat for more.
    #[arg(long, short = 'f')]
    pub field: Vec<String>,
}

// ---- release ----

#[derive(Subcommand, Debug)]
pub enum ReleaseCmd {
    Create(ReleaseCreateArgs),
    List(ReleaseListArgs),
    /// Upload files to a release.
    Upload(ReleaseUploadArgs),
}

#[derive(Args, Debug)]
pub struct ReleaseCreateArgs {
    pub tag: String,
    #[arg(long, short)]
    pub title: Option<String>,
    #[arg(long, short)]
    pub notes: Option<String>,
    #[arg(long, short = 'F', conflicts_with = "notes")]
    pub notes_file: Option<String>,
    /// Write the notes from the commits since the last release.
    #[arg(long)]
    pub generate_notes: bool,
    /// The branch or commit the tag is created on, if the tag does not exist yet.
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long)]
    pub draft: bool,
    #[arg(long)]
    pub prerelease: bool,
    /// Files to upload to the new release.
    pub files: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ReleaseListArgs {
    #[arg(long, short = 'L', default_value_t = 30)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ReleaseUploadArgs {
    pub tag: String,
    #[arg(required = true)]
    pub files: Vec<String>,
    /// Replace an asset that has the same name.
    #[arg(long)]
    pub clobber: bool,
}

// ---- api ----

#[derive(Args, Debug)]
pub struct ApiArgs {
    /// A path such as `/repos/alice/demo`, relative to `/api/v3`.
    pub path: String,
    #[arg(long, short = 'X', default_value = "GET")]
    pub method: String,
    /// A string field, as `key=value`. Sent as JSON, or as the query string for GET.
    #[arg(long, short = 'f')]
    pub field: Vec<String>,
    /// A typed field, as `key=value`: `true`, `false`, `null`, numbers and `@file` are not strings.
    #[arg(long, short = 'F')]
    pub raw_field: Vec<String>,
    /// A request header, as `Name: value`.
    #[arg(long, short = 'H')]
    pub header: Vec<String>,
    /// Send this file as the body (`-` for standard input).
    #[arg(long)]
    pub input: Option<String>,
    /// Print the status line and headers too.
    #[arg(long, short)]
    pub include: bool,
    /// Follow `Link` headers and print every page as one JSON array.
    #[arg(long)]
    pub paginate: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap checks its own rules (duplicate short flags and the like) only when it builds the
    /// parser, which would otherwise be the first time someone runs the command.
    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }
}
