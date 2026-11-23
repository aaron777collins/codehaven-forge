//! Search over repositories, issues and pull requests, users and code.
//!
//! Repositories and issues use the Postgres text search columns the schema already keeps,
//! plus trigram matching for names. Results are filtered through `Authz`, never through SQL
//! that copies the permission model, so a private repository cannot leak into a result.

use crate::core::Core;
use crate::filter::Spec;
use crate::util;
use ch_core::model::*;
use ch_core::{Actor, Permission, RepoId, Result, UserId};

const MAX_QUERY: usize = 200;

fn clean(q: &str) -> String {
    q.trim().chars().filter(|c| *c != '\0').take(MAX_QUERY).collect()
}

fn excerpt(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 160 { flat } else { format!("{}...", flat.chars().take(160).collect::<String>().trim_end()) }
}

impl Core {
    pub async fn search_repos(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        // Signed out sees public repositories only, filtered in SQL before paging so private
        // rows are never even read for a stranger.
        let only_public = if actor.is_anonymous() { "AND r.access_tier = 'public'" } else { "" };
        let q = clean(query);
        let want = limit.clamp(1, 1000) as usize;
        let like = format!("%{}%", util::like_escape(&q));
        let batch = (want * 3).max(30) as i64;
        let mut out: Vec<SearchHit> = Vec::new();
        let mut offset = 0i64;
        for _ in 0..5 {
            let rows: Vec<(RepoId, String, String)> = if q.is_empty() {
                sqlx::query_as(&format!(
                    "SELECT r.id, u.username::text || '/' || r.name::text, r.description FROM repositories r JOIN users u ON u.id = r.owner_id
                     WHERE true {only_public}
                     ORDER BY r.updated_at DESC, r.id DESC LIMIT $1 OFFSET $2"
                ))
                .bind(batch)
                .bind(offset)
                .fetch_all(self.db())
                .await?
            } else {
                sqlx::query_as(&format!(
                    "SELECT r.id, u.username::text || '/' || r.name::text, r.description
                     FROM repositories r JOIN users u ON u.id = r.owner_id
                     WHERE (r.search @@ plainto_tsquery('simple', $1) OR r.name::text ILIKE $2 ESCAPE '\\' OR (u.username::text || '/' || r.name::text) ILIKE $2 ESCAPE '\\'
                           OR r.name::text % $1 OR r.description ILIKE $2 ESCAPE '\\') {only_public}
                     ORDER BY (ts_rank(r.search, plainto_tsquery('simple', $1)) + similarity(r.name::text, $1)) DESC, r.updated_at DESC, r.id DESC
                     LIMIT $3 OFFSET $4"
                ))
                .bind(&q)
                .bind(&like)
                .bind(batch)
                .bind(offset)
                .fetch_all(self.db())
                .await?
            };
            let n = rows.len() as i64;
            offset += n;
            let ok = self.readable_at(actor, rows.iter().map(|r| r.0).collect(), Permission::View).await?;
            for (id, name, desc) in rows {
                if ok.contains(&id) {
                    out.push(SearchHit { kind: "repo".into(), id: id.0, title: name, snippet: excerpt(&desc) });
                }
            }
            if out.len() >= want || n < batch {
                break;
            }
        }
        out.truncate(want);
        Ok(out)
    }

    pub async fn search_issues(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        if actor.is_anonymous() {
            return Ok(Vec::new());
        }
        let want = limit.clamp(1, 1000) as usize;
        let spec = Spec { filter: IssueFilter { query: Some(clean(query)), ..Default::default() }, rank: true, ..Default::default() };
        let batch = (want * 3).max(30) as i64;
        let mut out: Vec<SearchHit> = Vec::new();
        let mut offset = 0i64;
        for _ in 0..5 {
            let rows = self.query_issue_rows(actor, &spec, offset, batch).await?;
            let n = rows.len() as i64;
            offset += n;
            let ok = self.readable(actor, rows.iter().map(|r| r.repo_id).collect()).await?;
            for r in rows.into_iter().filter(|r| ok.contains(&r.repo_id)) {
                out.push(SearchHit {
                    kind: if r.is_pull { "pull" } else { "issue" }.into(),
                    id: r.id.0,
                    title: r.title,
                    snippet: excerpt(&r.body),
                });
            }
            if out.len() >= want || n < batch {
                break;
            }
        }
        out.truncate(want);
        Ok(out)
    }

    pub async fn search_users(&self, actor: &Actor, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        if actor.is_anonymous() {
            return Ok(Vec::new());
        }
        let q = clean(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let like = format!("%{}%", util::like_escape(&q));
        let rows: Vec<(UserId, String, String)> = sqlx::query_as(
            "SELECT id, username::text, display_name FROM users
             WHERE suspended_at IS NULL AND kind <> 'bot' AND (username::text ILIKE $1 ESCAPE '\\' OR display_name ILIKE $1 ESCAPE '\\' OR username::text % $2)
             ORDER BY (username::text ILIKE $3 ESCAPE '\\') DESC, similarity(username::text, $2) DESC, username LIMIT $4",
        )
        .bind(&like)
        .bind(&q)
        .bind(format!("{}%", util::like_escape(&q)))
        .bind(limit.clamp(1, 1000) as i64)
        .fetch_all(self.db())
        .await?;
        Ok(rows.into_iter().map(|(id, name, display)| SearchHit { kind: "user".into(), id: id.0, title: name, snippet: display }).collect())
    }

    pub async fn search_code(&self, actor: &Actor, repo: RepoId, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let perm = self.permission(actor, repo).await?;
        if perm < Permission::Read {
            return Ok(Vec::new());
        }
        let q = clean(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let r = self.repo(repo).await?;
        self.d.git.grep(repo, &r.default_branch, &q, limit.clamp(1, 1000)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts_flatten_and_cut() {
        assert_eq!(excerpt("a\n\n b   c"), "a b c");
        let long = "word ".repeat(100);
        let e = excerpt(&long);
        assert!(e.ends_with("..."));
        assert!(e.chars().count() <= 164);
    }

    #[test]
    fn queries_are_trimmed_and_capped() {
        assert_eq!(clean("  hi "), "hi");
        assert_eq!(clean(&"x".repeat(500)).len(), 200);
    }
}
