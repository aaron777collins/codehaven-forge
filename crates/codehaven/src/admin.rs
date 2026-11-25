//! The `codehaven admin` subcommands. They call the same service traits the web uses, as
//! `Actor::System`, so every one writes its audit event through the trait ("System (cli)").
//! The password never passes through a shell: the default is a one-time link.

use crate::cli::{AdminCommand, CreateUser, InviteCommand, PublicReposCommand, parse_expiry};
use anyhow::{Context, Result, bail};
use ch_core::identity::UserListFilter;
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, Error, InviteId};
use std::io::Read;

fn describe(e: Error) -> anyhow::Error {
    match e {
        Error::Unavailable(m) => anyhow::anyhow!("{m}"),
        other => anyhow::anyhow!("{}", other.public_message()),
    }
}

pub async fn run(state: &AppState, command: AdminCommand) -> Result<()> {
    let sys = Actor::System;
    let cli_client = ClientInfo::local();
    let base = &state.config.server.external_url;
    match command {
        AdminCommand::CreateUser(CreateUser { username, email, admin, can_run_ci, password_stdin }) => {
            let user = state
                .accounts
                .create_user(
                    &sys,
                    NewUser { username: username.clone(), email, display_name: None, is_admin: admin, can_run_ci },
                    &cli_client,
                )
                .await
                .map_err(describe)?;
            if password_stdin {
                let mut pw = String::new();
                std::io::stdin().read_to_string(&mut pw).context("reading the password from standard input")?;
                let pw = pw.trim_end_matches(['\n', '\r']);
                state.accounts.set_password(&sys, user.id, pw, &cli_client).await.map_err(describe)?;
                println!("Created {username} (id {}) with the password from standard input.", user.id);
            } else {
                let link = state.accounts.issue_link(&sys, user.id, LinkPurpose::SetPassword, 60, &cli_client).await.map_err(describe)?;
                println!("Created {username} (id {}).", user.id);
                println!("Set a password here. The link works once and expires at {}:", link.expires_at.format("%Y-%m-%d %H:%M UTC"));
                println!("{base}{}", link.path);
            }
        }
        AdminCommand::Invite { command } => match command {
            InviteCommand::Create { email, admin, can_run_ci, org, team, expires, note } => {
                let (org_id, team_id) = match org {
                    Some(name) => {
                        let o = state.orgs.get(&sys, &name).await.map_err(describe)?;
                        let t = match team {
                            Some(t) => Some(state.orgs.get_team(&sys, o.id, &t).await.map_err(describe)?.id),
                            None => None,
                        };
                        (Some(o.id), t)
                    }
                    None => (None, None),
                };
                let hours = parse_expiry(&expires).map_err(|e| anyhow::anyhow!(e))?;
                let min = state.config.invites.min_expiry_hours;
                let max = state.config.invites.max_expiry_hours;
                if hours < min || hours > max {
                    bail!("expiry must be between {min}h and {max}h");
                }
                let created = state
                    .invites
                    .create(
                        &sys,
                        NewInvite {
                            email,
                            grants_admin: admin,
                            grants_ci: can_run_ci,
                            org_id,
                            team_id,
                            note,
                            expires_in_hours: Some(hours),
                        },
                        &cli_client,
                    )
                    .await
                    .map_err(describe)?;
                println!(
                    "Invite {} works once and expires at {}:",
                    created.invite.id,
                    created.invite.expires_at.format("%Y-%m-%d %H:%M UTC")
                );
                println!("{}", created.url);
            }
            InviteCommand::List => {
                let now = chrono::Utc::now();
                for i in state.invites.list(&sys).await.map_err(describe)? {
                    println!(
                        "{:>5}  {:<8}  expires {}  {}",
                        i.id,
                        format!("{:?}", i.status(now)).to_lowercase(),
                        i.expires_at.format("%Y-%m-%d %H:%M"),
                        i.email.as_deref().unwrap_or("-")
                    );
                }
            }
            InviteCommand::Revoke { id } => {
                state.invites.revoke(&sys, InviteId(id), &cli_client).await.map_err(describe)?;
                println!("Revoked invite {id}.");
            }
        },
        AdminCommand::RunnerToken { repo } => {
            let repo_id = match repo {
                Some(full) => {
                    let (owner, name) = full.split_once('/').context("--repo must look like owner/name")?;
                    Some(state.repos.get_by_name(&sys, owner, name).await.map_err(describe)?.id)
                }
                None => None,
            };
            let t = state.ci.create_runner_token(&sys, repo_id, &cli_client).await.map_err(describe)?;
            println!("Registration token. It works once and expires at {}:", t.expires_at.format("%Y-%m-%d %H:%M UTC"));
            println!("{}", t.token);
        }
        AdminCommand::Runners => {
            for r in state.pipelines.runners(&sys, None).await.map_err(describe)? {
                let seen = r.last_seen_at.map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|| "never".into());
                println!("{:>5}  {:<24} {:<8} last seen {}  {}", r.id.0, r.name, r.status, seen, r.labels.join(","));
            }
        }
        AdminCommand::RevokeRunner { id } => {
            state.pipelines.revoke_runner(&sys, ch_core::RunnerId(id), &cli_client).await.map_err(describe)?;
            println!("Removed runner {id}. Its credential no longer works.");
        }
        AdminCommand::ResetPassword { username } => {
            let user = state.accounts.get_by_username(&sys, &username).await.map_err(describe)?;
            state.accounts.sign_out_everywhere(&sys, user.id, &cli_client).await.map_err(describe)?;
            let link = state.accounts.issue_link(&sys, user.id, LinkPurpose::ResetPassword, 60, &cli_client).await.map_err(describe)?;
            println!(
                "Signed {username} out everywhere and revoked their access tokens. Set a new password here. The link works once and expires at {}:",
                link.expires_at.format("%Y-%m-%d %H:%M UTC")
            );
            println!("{base}{}", link.path);
        }
        AdminCommand::Unlock { username } => {
            let user = state.accounts.get_by_username(&sys, &username).await.map_err(describe)?;
            state.accounts.unlock(&sys, user.id, &cli_client).await.map_err(describe)?;
            println!("Unlocked {username}.");
        }
        AdminCommand::Unsuspend { username } => {
            let user = state.accounts.get_by_username(&sys, &username).await.map_err(describe)?;
            state.directory.unsuspend(&sys, user.id, &cli_client).await.map_err(describe)?;
            println!("Unsuspended {username}.");
        }
        AdminCommand::Users => {
            let users = state.directory.list_users(&sys, UserListFilter { query: None, page: 1, per_page: 100 }).await.map_err(describe)?;
            for u in users {
                let mut flags = Vec::new();
                if u.is_admin {
                    flags.push("admin");
                }
                if u.has_2fa {
                    flags.push("2fa");
                }
                if u.can_run_ci {
                    flags.push("ci");
                }
                if u.suspended {
                    flags.push("suspended");
                }
                if !u.has_password {
                    flags.push("no-password");
                }
                println!("{:>5}  {:<24} {:<32} {}", u.id, u.username, u.email.as_deref().unwrap_or("-"), flags.join(","));
            }
        }
        AdminCommand::PublicRepos { command } => match command {
            PublicReposCommand::Status => {
                let on = state.repos.public_repos_allowed().await.map_err(describe)?;
                println!("Public repositories are {}.", if on { "allowed" } else { "off" });
                if on {
                    let public = state.repos.list_accessible(&Actor::Anonymous, 1, 100).await.map_err(describe)?;
                    if public.is_empty() {
                        println!("No repository is public.");
                    }
                    for r in public {
                        println!("public  {}", r.full_name());
                    }
                }
            }
            PublicReposCommand::On => {
                state.repos.set_public_repos_allowed(&sys, true, &cli_client).await.map_err(describe)?;
                println!("Public repositories are allowed. No repository was made public: an owner does that in the browser.");
            }
            PublicReposCommand::Off => {
                let n = state.repos.set_public_repos_allowed(&sys, false, &cli_client).await.map_err(describe)?;
                println!(
                    "Public repositories are off. {n} public {} internal now.",
                    if n == 1 { "repository is" } else { "repositories are" }
                );
            }
        },
        AdminCommand::RepoImport { owner, name, from, run_ci } => {
            let user = state.accounts.get_by_username(&sys, &owner).await.map_err(describe)?;
            let repo = state
                .repos
                .import_local(
                    &sys,
                    NewRepo {
                        owner_id: user.id,
                        name: name.clone(),
                        description: String::new(),
                        visibility: Visibility::Private,
                        default_branch: None,
                        auto_init: false,
                    },
                    &from,
                    run_ci,
                    &cli_client,
                )
                .await
                .map_err(describe)?;
            println!(
                "Created the private repository {} from {from}{}.",
                repo.full_name(),
                if run_ci { " and started its workflows" } else { "" }
            );
        }
        AdminCommand::Suspend { username } => {
            let user = state.accounts.get_by_username(&sys, &username).await.map_err(describe)?;
            state.accounts.suspend(&sys, user.id, &cli_client).await.map_err(describe)?;
            println!("Suspended {username}.");
        }
    }
    Ok(())
}
