//! Users, organizations, teams and the credentials attached to an account.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::common::SimpleUser;

/// `GET /users/{login}` and `GET /user`: the full profile of an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub login: String,
    pub id: i64,
    pub avatar_url: String,
    pub url: String,
    pub html_url: String,
    /// `User`, `Organization` or `Bot`.
    #[serde(rename = "type")]
    pub kind: String,
    pub site_admin: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub bio: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Only present when the signed-in account asks about itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub two_factor_authentication: Option<bool>,
    /// Set while the account is suspended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_at: Option<DateTime<Utc>>,
}

/// `GET /orgs/{org}`: an organization account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Organization {
    pub login: String,
    pub id: i64,
    pub url: String,
    pub html_url: String,
    pub avatar_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// `GET /orgs/{org}/teams/{slug}`: a team inside an organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: i64,
    pub name: String,
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The base permission team members get on team repos: `pull`, `triage`,
    /// `push`, `maintain` or `admin`.
    pub permission: String,
    pub url: String,
    pub html_url: String,
    pub members_count: i64,
    pub repos_count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<Organization>,
}

/// The role an account holds in an organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgRole {
    Member,
    Admin,
}

/// `GET /user/memberships/orgs/{org}`: the membership of an account in an organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgMembership {
    /// `active` or `pending`.
    pub state: String,
    pub role: OrgRole,
    pub url: String,
    pub user: SimpleUser,
    pub organization: Organization,
}

/// `GET /user/keys` and `GET /users/{login}/keys`: an SSH public key on an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicKey {
    pub id: i64,
    pub key: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    pub read_only: bool,
    pub verified: bool,
    pub url: String,
}

/// `POST /user/keys`: upload an SSH public key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateKey {
    pub title: String,
    pub key: String,
}

/// `GET /user/emails`: an email address on the signed-in account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Email {
    pub email: String,
    pub primary: bool,
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
}

/// `POST /user/emails`: attach email addresses to the signed-in account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddEmails {
    #[serde(default)]
    pub emails: Vec<String>,
}

/// `POST /user/tokens`: mint a personal access token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateToken {
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_days: Option<u32>,
}

/// `GET /user/tokens`, and the response to `POST /user/tokens`: a personal access token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonalToken {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub token_last_eight: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    /// Only present in the response to `POST /user/tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// `PATCH /user`: edit the signed-in profile. Absent fields stay unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateUser {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub company: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// `POST /user/orgs`: create an organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateOrg {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `POST /orgs/{org}/teams`: create a team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTeam {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
}

/// `PATCH /orgs/{org}/teams/{slug}`. Only `permission` can change. A `name` or `description`
/// that differs from the team's is refused rather than ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateTeam {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org() -> Organization {
        Organization {
            login: "haven".into(),
            id: 20,
            url: "https://code.example.com/api/v3/orgs/haven".into(),
            html_url: "https://code.example.com/orgs/haven".into(),
            avatar_url: "https://code.example.com/avatar/haven.svg".into(),
            description: Some("the forge collective".into()),
            name: Some("Haven".into()),
            created_at: crate::t0(),
        }
    }

    #[test]
    fn user_roundtrip() {
        let value = User {
            login: "alice".into(),
            id: 1,
            avatar_url: "https://code.example.com/avatar/alice.svg".into(),
            url: "https://code.example.com/api/v3/users/alice".into(),
            html_url: "https://code.example.com/alice".into(),
            kind: "User".into(),
            site_admin: true,
            name: Some("Alice".into()),
            email: Some("alice@example.com".into()),
            bio: Some("builds the forge".into()),
            company: Some("Haven".into()),
            location: Some("NZ".into()),
            created_at: crate::t0(),
            updated_at: crate::t0(),
            two_factor_authentication: Some(true),
            suspended_at: None,
        };
        crate::roundtrip("users_user", &value);
    }

    #[test]
    fn organization_roundtrip() {
        crate::roundtrip("users_organization", &org());
    }

    #[test]
    fn team_roundtrip() {
        let value = Team {
            id: 30,
            name: "Core".into(),
            slug: "core".into(),
            description: Some("forge maintainers".into()),
            permission: "push".into(),
            url: "https://code.example.com/api/v3/orgs/haven/teams/core".into(),
            html_url: "https://code.example.com/orgs/haven/teams/core".into(),
            members_count: 2,
            repos_count: 3,
            organization: Some(org()),
        };
        crate::roundtrip("users_team", &value);
    }

    #[test]
    fn org_membership_roundtrip() {
        let value = OrgMembership {
            state: "active".into(),
            role: OrgRole::Admin,
            url: "https://code.example.com/api/v3/user/memberships/orgs/haven".into(),
            user: crate::user("alice", 1),
            organization: org(),
        };
        crate::roundtrip("users_org_membership", &value);
    }

    #[test]
    fn public_key_roundtrip() {
        let value = PublicKey {
            id: 40,
            key: "ssh-ed25519 AAAAC3NzaC1lZDI1anNt test".into(),
            title: "laptop".into(),
            created_at: crate::t0(),
            last_used_at: Some(crate::t0()),
            read_only: true,
            verified: false,
            url: "https://code.example.com/api/v3/user/keys/40".into(),
        };
        crate::roundtrip("users_public_key", &value);
    }

    #[test]
    fn create_key_roundtrip() {
        let value = CreateKey { title: "laptop".into(), key: "ssh-ed25519 AAAAC3NzaC1lZDI1anNt test".into() };
        crate::roundtrip("users_create_key", &value);
    }

    #[test]
    fn email_roundtrip() {
        let value = Email { email: "alice@example.com".into(), primary: true, verified: true, visibility: Some("public".into()) };
        crate::roundtrip("users_email", &value);
    }

    #[test]
    fn add_emails_roundtrip() {
        let value = AddEmails { emails: vec!["alice@example.com".into(), "alice@example.org".into()] };
        crate::roundtrip("users_add_emails", &value);
    }

    #[test]
    fn create_token_roundtrip() {
        let value = CreateToken { name: "ci token".into(), scopes: vec!["repo".into(), "read:org".into()], expires_in_days: Some(30) };
        crate::roundtrip("users_create_token", &value);
    }

    #[test]
    fn personal_token_roundtrip() {
        let value = PersonalToken {
            id: 50,
            name: "ci token".into(),
            scopes: vec!["repo".into()],
            token_last_eight: "abcd1234".into(),
            expires_at: crate::t0(),
            created_at: crate::t0(),
            last_used_at: None,
            token: Some("haven_tok_9f8e7d6c".into()),
        };
        crate::roundtrip("users_personal_token", &value);
    }

    #[test]
    fn update_user_roundtrip() {
        let value = UpdateUser {
            name: Some("Alice".into()),
            email: None,
            bio: Some("builds the forge".into()),
            company: None,
            location: Some("NZ".into()),
        };
        crate::roundtrip("users_update_user", &value);
    }

    #[test]
    fn create_org_roundtrip() {
        let value = CreateOrg { name: "haven".into(), description: Some("the forge collective".into()) };
        crate::roundtrip("users_create_org", &value);
    }

    #[test]
    fn create_team_roundtrip() {
        let value = CreateTeam { name: "Core".into(), description: Some("forge maintainers".into()), permission: None };
        crate::roundtrip("users_create_team", &value);
    }

    #[test]
    fn user_from_json_ignores_unknown_fields() {
        let raw = r#"{
            "login": "alice",
            "id": 1,
            "node_id": "MDQ6VXNlcjE=",
            "avatar_url": "https://code.example.com/avatar/alice.svg",
            "gravatar_id": "",
            "url": "https://code.example.com/api/v3/users/alice",
            "html_url": "https://code.example.com/alice",
            "type": "User",
            "site_admin": true,
            "name": "Alice",
            "company": "Haven",
            "blog": "https://code.example.com/blog",
            "location": "NZ",
            "email": "alice@example.com",
            "hireable": null,
            "bio": "builds the forge",
            "twitter_username": null,
            "public_repos": 3,
            "public_gists": 0,
            "followers": 1,
            "following": 1,
            "created_at": "2026-01-02T03:04:05Z",
            "updated_at": "2026-02-03T04:05:06Z",
            "suspended_at": null,
            "two_factor_authentication": true,
            "private_gists": 0,
            "total_private_repos": 0,
            "owned_private_repos": 0,
            "disk_usage": 0,
            "collaborators": 0
        }"#;
        let value: User = serde_json::from_str(raw).expect("parse user json");
        assert_eq!(value.login, "alice");
        assert_eq!(value.kind, "User");
        assert_eq!(value.bio, Some("builds the forge".into()));
    }
}
