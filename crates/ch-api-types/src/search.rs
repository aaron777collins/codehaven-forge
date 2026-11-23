//! Search results: the shared envelope and the item shapes it can carry.

use serde::{Deserialize, Serialize};

use crate::common::SimpleUser;
use crate::repos::Repository;

/// The envelope every `GET /search/*` endpoint responds with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult<T> {
    pub total_count: i64,
    pub incomplete_results: bool,
    // `default = "Vec::new"`, not a bare `default`: that would demand `T: Default`.
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
}

/// A matched fragment, present when `GET /search/code` is called with the
/// `text-match` media type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMatch {
    pub fragment: String,
}

/// One hit of `GET /search/code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSearchItem {
    pub name: String,
    pub path: String,
    pub sha: String,
    pub url: String,
    pub html_url: String,
    pub repository: Repository,
    #[serde(default)]
    pub text_matches: Vec<TextMatch>,
}

/// One hit of `GET /search/users`.
pub type UserSearchItem = SimpleUser;

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repository {
        Repository {
            id: 10,
            name: "codehaven".into(),
            full_name: "alice/codehaven".into(),
            owner: crate::user("alice", 1),
            private: false,
            html_url: "https://code.example.com/alice/codehaven".into(),
            description: Some("a self-hosted forge".into()),
            fork: false,
            url: "https://code.example.com/api/v3/repos/alice/codehaven".into(),
            clone_url: "https://code.example.com/alice/codehaven.git".into(),
            ssh_url: "git@code.example.com:alice/codehaven.git".into(),
            default_branch: "main".into(),
            visibility: "public".into(),
            contribution_policy: "everyone".into(),
            archived: false,
            disabled: false,
            topics: vec!["rust".into()],
            has_issues: true,
            created_at: crate::t0(),
            updated_at: crate::t0(),
            pushed_at: Some(crate::t0()),
            size: 42,
            stargazers_count: 1,
            watchers_count: 1,
            forks_count: 0,
            open_issues_count: 0,
            permissions: None,
            parent: None,
            empty: false,
            allow_squash_merge: true,
            allow_merge_commit: true,
            allow_rebase_merge: true,
            delete_branch_on_merge: false,
        }
    }

    fn code_item() -> CodeSearchItem {
        CodeSearchItem {
            name: "lib.rs".into(),
            path: "src/lib.rs".into(),
            sha: "b10b1234".into(),
            url: "https://code.example.com/api/v3/repos/alice/codehaven/contents/src/lib.rs".into(),
            html_url: "https://code.example.com/alice/codehaven/blob/main/src/lib.rs".into(),
            repository: repo(),
            text_matches: vec![TextMatch { fragment: "pub mod search;".into() }],
        }
    }

    #[test]
    fn search_result_roundtrip() {
        let value = SearchResult { total_count: 2, incomplete_results: false, items: vec![crate::user("alice", 1), crate::user("kim", 2)] };
        crate::roundtrip("search_search_result", &value);
    }

    #[test]
    fn code_search_item_roundtrip() {
        crate::roundtrip("search_code_search_item", &code_item());
    }

    #[test]
    fn search_result_code_roundtrip() {
        let value = SearchResult { total_count: 1, incomplete_results: true, items: vec![code_item()] };
        crate::roundtrip("search_search_result_code", &value);
    }

    #[test]
    fn code_search_from_json_ignores_unknown_fields() {
        let raw = r#"{
            "total_count": 1,
            "incomplete_results": false,
            "repository_count": 1,
            "items": [
                {
                    "name": "lib.rs",
                    "path": "src/lib.rs",
                    "sha": "b10b1234",
                    "node_id": "ABC_def",
                    "url": "https://code.example.com/api/v3/repos/alice/codehaven/contents/src/lib.rs",
                    "git_url": "https://code.example.com/api/v3/repos/alice/codehaven/git/blobs/b10b1234",
                    "html_url": "https://code.example.com/alice/codehaven/blob/main/src/lib.rs",
                    "score": 1.0,
                    "repository": {
                        "id": 10,
                        "node_id": "R_kgDO",
                        "name": "codehaven",
                        "full_name": "alice/codehaven",
                        "private": false,
                        "owner": {
                            "login": "alice",
                            "id": 1,
                            "avatar_url": "https://code.example.com/avatars/1.svg",
                            "url": "https://code.example.com/api/v3/users/alice",
                            "html_url": "https://code.example.com/alice",
                            "type": "User",
                            "site_admin": false
                        },
                        "html_url": "https://code.example.com/alice/codehaven",
                        "description": "a self-hosted forge",
                        "fork": false,
                        "url": "https://code.example.com/api/v3/repos/alice/codehaven",
                        "clone_url": "https://code.example.com/alice/codehaven.git",
                        "ssh_url": "git@code.example.com:alice/codehaven.git",
                        "default_branch": "main",
                        "visibility": "public",
                        "archived": false,
                        "disabled": false,
                        "topics": ["rust"],
                        "has_issues": true,
                        "has_wiki": false,
                        "created_at": "2026-01-02T03:04:05Z",
                        "updated_at": "2026-02-03T04:05:06Z",
                        "pushed_at": "2026-02-03T04:05:06Z",
                        "size": 42,
                        "stargazers_count": 1,
                        "watchers_count": 1,
                        "forks_count": 0,
                        "open_issues_count": 0,
                        "license": null,
                        "empty": false,
                        "allow_squash_merge": true,
                        "allow_merge_commit": true,
                        "allow_rebase_merge": true,
                        "delete_branch_on_merge": false
                    },
                    "text_matches": [
                        {
                            "fragment": "pub mod search;",
                            "property": "content",
                            "object_url": "https://code.example.com/alice/codehaven/blob/main/src/lib.rs",
                            "matches": [{"indices": [0, 7], "text": "pub mod"}]
                        }
                    ]
                }
            ]
        }"#;
        let value: SearchResult<CodeSearchItem> = serde_json::from_str(raw).expect("parse code search json");
        assert_eq!(value.total_count, 1);
        assert_eq!(value.items[0].name, "lib.rs");
        assert_eq!(value.items[0].repository.full_name, "alice/codehaven");
        assert_eq!(value.items[0].text_matches[0].fragment, "pub mod search;");
    }
}
