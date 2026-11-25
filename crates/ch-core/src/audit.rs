//! Audit events (ARCHITECTURE.md section 7.7).

use crate::ids::*;
use crate::model::ClientInfo;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Success,
    Failure,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
        }
    }
}

/// Every action name the audit log may hold. One list, so the log stays searchable and a
/// typo is a compile error.
pub mod action {
    pub const SIGN_IN: &str = "auth.sign_in";
    pub const SIGN_IN_FAILED: &str = "auth.sign_in_failed";
    pub const TWO_FACTOR_FAILED: &str = "auth.two_factor_failed";
    pub const LOCKOUT: &str = "auth.lockout";
    pub const UNLOCK: &str = "auth.unlock";
    pub const SIGN_OUT: &str = "auth.sign_out";
    pub const PASSWORD_SET: &str = "auth.password_set";
    pub const PASSWORD_CHANGED: &str = "auth.password_changed";
    pub const PASSWORD_RESET: &str = "auth.password_reset";
    pub const TWO_FACTOR_ON: &str = "auth.two_factor_on";
    pub const TWO_FACTOR_OFF: &str = "auth.two_factor_off";
    pub const RECOVERY_CODE_USED: &str = "auth.recovery_code_used";
    pub const INVITE_CREATED: &str = "invite.created";
    pub const INVITE_REVOKED: &str = "invite.revoked";
    pub const INVITE_ACCEPTED: &str = "invite.accepted";
    pub const INVITE_REFUSED: &str = "invite.refused";
    pub const USER_CREATED: &str = "user.created";
    pub const USER_SUSPENDED: &str = "user.suspended";
    pub const USER_ADMIN_GRANTED: &str = "user.admin_granted";
    pub const USER_CI_CHANGED: &str = "user.can_run_ci_changed";
    pub const SESSION_REVOKED: &str = "session.revoked";
    pub const TOKEN_CREATED: &str = "token.created";
    pub const TOKEN_DELETED: &str = "token.deleted";
    pub const SSH_KEY_ADDED: &str = "ssh_key.added";
    pub const SSH_KEY_REMOVED: &str = "ssh_key.removed";
    pub const RUNNER_TOKEN_ISSUED: &str = "runner.token_issued";
    pub const RUNNER_REGISTERED: &str = "runner.registered";
    pub const RUNNER_REMOVED: &str = "runner.removed";
    pub const ACTIONS_TOKEN_CEILING: &str = "repo.actions_token_ceiling_changed";
    pub const ACTIONS_POLICY: &str = "instance.actions_policy_changed";
    pub const SECRET_CHANGED: &str = "secret.changed";
    pub const SECRET_DELETED: &str = "secret.deleted";
    pub const REPO_VISIBILITY: &str = "repo.visibility_changed";
    pub const REPO_TRANSFERRED: &str = "repo.transferred";
    pub const REPO_DELETED: &str = "repo.deleted";
    pub const BRANCH_PROTECTION: &str = "repo.branch_protection_changed";
    pub const USER_UNSUSPENDED: &str = "user.unsuspended";
    pub const EMAIL_ADDED: &str = "email.added";
    pub const EMAIL_REMOVED: &str = "email.removed";
    pub const ORG_CREATED: &str = "org.created";
    pub const ORG_UPDATED: &str = "org.updated";
    pub const ORG_DELETED: &str = "org.deleted";
    pub const ORG_MEMBER_SET: &str = "org.member_set";
    pub const ORG_MEMBER_REMOVED: &str = "org.member_removed";
    pub const TEAM_CREATED: &str = "team.created";
    pub const TEAM_UPDATED: &str = "team.updated";
    pub const TEAM_DELETED: &str = "team.deleted";
    pub const TEAM_MEMBER_ADDED: &str = "team.member_added";
    pub const TEAM_MEMBER_REMOVED: &str = "team.member_removed";
    pub const TEAM_REPO_SET: &str = "team.repo_set";
    pub const TEAM_REPO_REMOVED: &str = "team.repo_removed";
    pub const WEBHOOK_CREATED: &str = "webhook.created";
    pub const WEBHOOK_UPDATED: &str = "webhook.updated";
    pub const WEBHOOK_DELETED: &str = "webhook.deleted";
    // Collaboration. Not in the section 7.7 list: merging code, removing someone's words and
    // publishing a release are the changes people ask "who did that" about.
    pub const PULL_MERGED: &str = "pull.merged";
    pub const PULL_AUTO_MERGE: &str = "pull.auto_merge_changed";
    pub const REVIEW_DISMISSED: &str = "review.dismissed";
    pub const COMMENT_DELETED: &str = "comment.deleted";
    pub const ISSUE_LOCKED: &str = "issue.lock_changed";
    pub const LABEL_DELETED: &str = "label.deleted";
    pub const RELEASE_PUBLISHED: &str = "release.published";
    pub const RELEASE_DELETED: &str = "release.deleted";
    // Connected apps (MCP clients such as Claude).
    pub const APP_AUTHORIZED: &str = "app.authorized";
    pub const APP_DENIED: &str = "app.denied";
    pub const APP_REVOKED: &str = "app.revoked";
    pub const APP_REFRESH_REUSE: &str = "app.refresh_reuse";
    pub const APP_CODE_REUSE: &str = "app.code_reuse";
    pub const MCP_TOOL_CALL: &str = "mcp.tool_call";
    pub const RUN_APPROVED: &str = "run.approved";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// `user`, `token`, `job`, `runner`, `system`, `anonymous`.
    pub actor_kind: String,
    pub actor_id: Option<i64>,
    /// Human readable, e.g. `System (cli)`.
    pub actor_label: String,
    pub client_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub outcome: Outcome,
    /// Names and ids only. Never a secret, a password or a token.
    pub details: serde_json::Value,
}

impl AuditEvent {
    pub fn new(actor: &crate::actor::Actor, client: &ClientInfo, action: &str, outcome: Outcome) -> Self {
        let (kind, id) = match actor {
            crate::actor::Actor::Anonymous => ("anonymous", None),
            crate::actor::Actor::User { id, .. } => ("user", Some(id.0)),
            crate::actor::Actor::Token { user_id, .. } => ("token", Some(user_id.0)),
            crate::actor::Actor::Job { job_id, .. } => ("job", Some(job_id.0)),
            crate::actor::Actor::Runner { runner_id } => ("runner", Some(runner_id.0)),
            crate::actor::Actor::App { user_id, .. } => ("app", Some(user_id.0)),
            crate::actor::Actor::System => ("system", None),
        };
        Self {
            actor_kind: kind.into(),
            actor_id: id,
            actor_label: actor.audit_label(),
            client_ip: Some(client.ip),
            user_agent: client.user_agent.clone(),
            action: action.into(),
            target_type: None,
            target_id: None,
            outcome,
            details: serde_json::Value::Null,
        }
    }
    pub fn target(mut self, kind: &str, id: impl ToString) -> Self {
        self.target_type = Some(kind.into());
        self.target_id = Some(id.to_string());
        self
    }
    pub fn details(mut self, v: serde_json::Value) -> Self {
        self.details = v;
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditFilter {
    pub actor_id: Option<UserId>,
    pub action: Option<String>,
    pub client_ip: Option<IpAddr>,
    pub since: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
    pub before_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAudit {
    pub id: AuditId,
    pub at: DateTime<Utc>,
    pub event: AuditEvent,
}
