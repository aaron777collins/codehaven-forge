//! Typed ids. Every table id is a `BIGINT`, and the API shows integers like GitHub's.
//! A newtype per table stops a `RepoId` being passed where a `UserId` is wanted.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($($(#[$m:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, sqlx::Type)]
        #[serde(transparent)]
        #[sqlx(transparent)]
        pub struct $name(pub i64);

        impl $name {
            pub const fn get(self) -> i64 { self.0 }
        }
        impl From<i64> for $name {
            fn from(v: i64) -> Self { Self(v) }
        }
        impl From<$name> for i64 {
            fn from(v: $name) -> i64 { v.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
    )+};
}

id_type!(
    /// A user, an organization or a bot. They share the `users` table.
    UserId,
    RepoId,
    OrgId,
    TeamId,
    TokenId,
    SessionId,
    SshKeyId,
    InviteId,
    IssueId,
    PullId,
    CommentId,
    ReviewId,
    LabelId,
    MilestoneId,
    ReleaseId,
    WebhookId,
    NotificationId,
    RunnerId,
    WorkflowId,
    RunId,
    JobId,
    PackageId,
    AuditId,
    /// One connected app: a person's consent for an MCP client such as Claude.
    GrantId,
);

impl RepoId {
    /// Directory shard for the bare repository: `data/git/<shard>/<id>.git`.
    pub fn shard(self) -> String {
        format!("{:02x}", self.0.rem_euclid(256))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_is_two_hex_digits() {
        assert_eq!(RepoId(1).shard(), "01");
        assert_eq!(RepoId(255).shard(), "ff");
        assert_eq!(RepoId(256).shard(), "00");
        assert_eq!(RepoId(1000).shard(), "e8");
    }

    #[test]
    fn ids_serialize_as_plain_integers() {
        assert_eq!(serde_json::to_string(&UserId(7)).unwrap(), "7");
        assert_eq!(serde_json::from_str::<RepoId>("9").unwrap(), RepoId(9));
    }
}
