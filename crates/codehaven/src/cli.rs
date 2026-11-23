//! Command line definition. See ARCHITECTURE.md section 14.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "codehaven", version, about = "Codehaven server and administration")]
pub struct Cli {
    /// Path to codehaven.toml. `CODEHAVEN__SECTION__KEY` variables override it.
    #[arg(long, short = 'c', global = true, env = "CODEHAVEN_CONFIG", default_value = "/etc/codehaven/codehaven.toml")]
    pub config: PathBuf,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the HTTP and SSH servers and the background workers.
    Serve {
        /// Apply pending migrations first.
        #[arg(long)]
        migrate: bool,
    },
    /// Apply pending database migrations and exit.
    Migrate,
    /// Account and instance administration. Runs inside the server container, so only
    /// someone with root on the host can use it.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Called by git, not by people.
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// Ask the internal listener whether the server is ready. Used by the compose healthcheck.
    Healthcheck,
    /// Check the installation: git version, paths, permissions, disk, database, public routes.
    Doctor,
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Create an account with no password and print a one-time set-password link (1 hour).
    CreateUser(CreateUser),
    /// Create, list or revoke invites.
    Invite {
        #[command(subcommand)]
        command: InviteCommand,
    },
    /// Print a single-use runner registration token (1 hour).
    RunnerToken {
        /// Limit the runner to one repository, as owner/name.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Print a fresh one-time link and sign the account out everywhere.
    ResetPassword { username: String },
    /// Clear a sign-in lockout.
    Unlock { username: String },
    /// Suspend an account: ends its sessions and disables its tokens.
    Suspend { username: String },
    /// Lift a suspension.
    Unsuspend { username: String },
    /// List accounts, newest last.
    Users,
    /// Whether repositories may be public (readable by anyone without an account). `off`
    /// lowers every public repository to internal at once; `on` makes none public.
    PublicRepos {
        #[command(subcommand)]
        command: PublicReposCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum PublicReposCommand {
    /// Say whether public repositories are allowed and list the public ones.
    Status,
    /// Allow public repositories. No repository becomes public by this.
    On,
    /// Turn public repositories off. Every public repository becomes internal.
    Off,
}

#[derive(Args, Debug)]
pub struct CreateUser {
    #[arg(long)]
    pub username: String,
    #[arg(long)]
    pub email: String,
    /// Make this a site admin. Admin pages still need two-factor.
    #[arg(long)]
    pub admin: bool,
    /// Allow this account's workflows to run.
    #[arg(long)]
    pub can_run_ci: bool,
    /// Read the password from standard input instead of printing a link. For the end-to-end
    /// harness. The password never appears in the process list or shell history.
    #[arg(long)]
    pub password_stdin: bool,
}

#[derive(Subcommand, Debug)]
pub enum InviteCommand {
    /// Create an invite link.
    Create {
        /// The account must use this address.
        #[arg(long)]
        email: Option<String>,
        /// The invite grants site admin.
        #[arg(long)]
        admin: bool,
        /// The invite allows the account's workflows to run.
        #[arg(long)]
        can_run_ci: bool,
        /// Put the new account in this organization.
        #[arg(long)]
        org: Option<String>,
        /// And in this team of that organization.
        #[arg(long, requires = "org")]
        team: Option<String>,
        /// How long it works: `1h` to `30d`. Default 7d.
        #[arg(long, default_value = "7d")]
        expires: String,
        /// A note to self, shown only in the admin list.
        #[arg(long)]
        note: Option<String>,
    },
    List,
    Revoke {
        id: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum HookCommand {
    PreReceive,
    PostReceive,
}

/// Parses `1h`, `36h`, `7d` into hours. Only whole hours and days.
pub fn parse_expiry(s: &str) -> Result<u32, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u32 = num.parse().map_err(|_| format!("`{s}` is not a duration like 12h or 7d"))?;
    match unit {
        "h" => Ok(n),
        "d" => n.checked_mul(24).ok_or_else(|| "too long".to_string()),
        _ => Err(format!("`{s}` is not a duration like 12h or 7d")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn expiry_parsing() {
        assert_eq!(parse_expiry("1h"), Ok(1));
        assert_eq!(parse_expiry("7d"), Ok(168));
        assert_eq!(parse_expiry("30d"), Ok(720));
        assert!(parse_expiry("7").is_err());
        assert!(parse_expiry("d").is_err());
        assert!(parse_expiry("7w").is_err());
        assert!(parse_expiry("").is_err());
    }

    #[test]
    fn parses_create_user() {
        let cli = Cli::try_parse_from(["codehaven", "admin", "create-user", "--username", "alice", "--email", "a@b.c", "--admin"]).unwrap();
        match cli.command {
            Command::Admin { command: AdminCommand::CreateUser(c) } => {
                assert_eq!(c.username, "alice");
                assert!(c.admin);
                assert!(!c.password_stdin);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn invite_team_needs_org() {
        assert!(Cli::try_parse_from(["codehaven", "admin", "invite", "create", "--team", "devs"]).is_err());
        assert!(Cli::try_parse_from(["codehaven", "admin", "invite", "create", "--org", "acme"]).is_ok());
        assert!(Cli::try_parse_from(["codehaven", "admin", "invite", "create", "--org", "acme", "--team", "devs"]).is_ok());
    }
}
