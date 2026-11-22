//! Labels and milestones.

use crate::core::{Core, MILESTONE_SELECT, MilestoneRow};
use crate::util;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::collab::*;
use ch_core::model::ClientInfo;
use ch_core::{Actor, Error, FieldError, LabelId, Permission, RepoId, Result};
use chrono::Utc;

/// Every new repository starts with these. The colors are ours.
pub const DEFAULT_LABELS: &[(&str, &str, &str)] = &[
    ("bug", "e11d48", "Something is not working"),
    ("enhancement", "0ea5e9", "A new feature or an improvement"),
    ("documentation", "6366f1", "Docs to write or fix"),
    ("question", "a855f7", "Needs an answer rather than a change"),
    ("good first issue", "16a34a", "A good place to start"),
    ("help wanted", "0d9488", "Extra hands would be welcome"),
    ("breaking", "f97316", "Changes that break compatibility"),
    ("duplicate", "71717a", "This already exists elsewhere"),
    ("wontfix", "52525b", "This will not be worked on"),
];

pub fn is_default_label(name: &str, _color: &str) -> bool {
    DEFAULT_LABELS.iter().any(|(n, _, _)| n.eq_ignore_ascii_case(name))
}

fn label_name(s: &str) -> Result<String> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Error::Validation(vec![FieldError::new("Label", "name", "missing_field")]));
    }
    if t.chars().count() > 50 {
        return Err(Error::invalid("Label", "name", "name is too long (maximum is 50 characters)"));
    }
    if t.chars().any(|c| c.is_control() || c == ',') {
        return Err(Error::invalid("Label", "name", "name cannot contain commas or control characters"));
    }
    Ok(t.to_string())
}

impl Core {
    // ------------------------------------------------------------ labels

    pub async fn labels(&self, actor: &Actor, repo: RepoId) -> Result<Vec<LabelInfo>> {
        self.need_read(actor, repo).await?;
        self.labels_of(repo).await
    }

    pub async fn labels_of(&self, repo: RepoId) -> Result<Vec<LabelInfo>> {
        let rows: Vec<(LabelId, String, String, String)> =
            sqlx::query_as("SELECT id, name::text, color, description FROM labels WHERE repo_id = $1 ORDER BY lower(name::text)")
                .bind(repo)
                .fetch_all(self.db())
                .await?;
        Ok(rows
            .into_iter()
            .map(|(id, name, color, description)| LabelInfo { is_default: is_default_label(&name, &color), id, name, color, description })
            .collect())
    }

    pub async fn label(&self, actor: &Actor, repo: RepoId, name: &str) -> Result<LabelInfo> {
        self.need_read(actor, repo).await?;
        let row: Option<(LabelId, String, String, String)> =
            sqlx::query_as("SELECT id, name::text, color, description FROM labels WHERE repo_id = $1 AND name = $2::citext")
                .bind(repo)
                .bind(name.trim())
                .fetch_optional(self.db())
                .await?;
        let (id, name, color, description) = row.ok_or(Error::NotFound)?;
        Ok(LabelInfo { is_default: is_default_label(&name, &color), id, name, color, description })
    }

    pub async fn create_label(&self, actor: &Actor, repo: RepoId, draft: LabelDraft, client: &ClientInfo) -> Result<LabelInfo> {
        self.need(actor, repo, Permission::Triage).await?;
        let name = label_name(&draft.name)?;
        let color = match &draft.color {
            Some(c) => util::color(c)?,
            None => "0f766e".to_string(),
        };
        let description = draft.description.unwrap_or_default();
        if description.chars().count() > 200 {
            return Err(Error::invalid("Label", "description", "description is too long (maximum is 200 characters)"));
        }
        let mut tx = self.db().begin().await?;
        let id: Option<LabelId> = sqlx::query_scalar(
            "INSERT INTO labels (repo_id, name, color, description) VALUES ($1, $2, $3, $4) ON CONFLICT (repo_id, name) DO NOTHING RETURNING id",
        )
        .bind(repo)
        .bind(&name)
        .bind(&color)
        .bind(&description)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            return Err(Error::Validation(vec![FieldError::new("Label", "name", "already_exists")]));
        };
        tx.commit().await?;
        let _ = client;
        Ok(LabelInfo { id, is_default: is_default_label(&name, &color), name, color, description })
    }

    pub async fn update_label(&self, actor: &Actor, repo: RepoId, name: &str, edit: LabelEdit, _client: &ClientInfo) -> Result<LabelInfo> {
        self.need(actor, repo, Permission::Triage).await?;
        let current = self.label(actor, repo, name).await?;
        let new_name = match &edit.new_name {
            Some(n) => label_name(n)?,
            None => current.name.clone(),
        };
        let color = match &edit.color {
            Some(c) => util::color(c)?,
            None => current.color.clone(),
        };
        let description = edit.description.clone().unwrap_or_else(|| current.description.clone());
        let res = sqlx::query("UPDATE labels SET name = $1::citext, color = $2, description = $3 WHERE id = $4")
            .bind(&new_name)
            .bind(&color)
            .bind(&description)
            .bind(current.id)
            .execute(self.db())
            .await;
        match res {
            Ok(_) => Ok(LabelInfo { id: current.id, is_default: is_default_label(&new_name, &color), name: new_name, color, description }),
            Err(e) if util::is_unique(&e) => Err(Error::Validation(vec![FieldError::new("Label", "name", "already_exists")])),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn delete_label(&self, actor: &Actor, repo: RepoId, name: &str, client: &ClientInfo) -> Result<()> {
        self.need(actor, repo, Permission::Triage).await?;
        let current = self.label(actor, repo, name).await?;
        let mut tx = self.db().begin().await?;
        sqlx::query("DELETE FROM labels WHERE id = $1").bind(current.id).execute(&mut *tx).await?;
        self.d
            .audit
            .record(
                &mut tx,
                AuditEvent::new(actor, client, action::LABEL_DELETED, Outcome::Success)
                    .target("repo", repo)
                    .details(serde_json::json!({ "label": current.name })),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Adds the default labels to a repository that has none of its own yet. Safe to repeat.
    pub async fn seed_default_labels(&self, repo: RepoId) -> Result<u64> {
        let mut added = 0;
        for (name, color, desc) in DEFAULT_LABELS {
            let r = sqlx::query(
                "INSERT INTO labels (repo_id, name, color, description) VALUES ($1, $2, $3, $4) ON CONFLICT (repo_id, name) DO NOTHING",
            )
            .bind(repo)
            .bind(name)
            .bind(color)
            .bind(desc)
            .execute(self.db())
            .await?;
            added += r.rows_affected();
        }
        Ok(added)
    }

    /// Looks up labels by name (case-insensitive) in one repository. A missing label is a
    /// validation error naming it, so a typo does not silently create a label.
    pub async fn resolve_labels(
        &self,
        tx: &mut ch_core::services::DbTx<'_>,
        repo: RepoId,
        names: &[String],
    ) -> Result<Vec<(LabelId, String, String)>> {
        let mut out: Vec<(LabelId, String, String)> = Vec::new();
        for n in names {
            let n = n.trim();
            if n.is_empty() {
                continue;
            }
            let row: Option<(LabelId, String, String)> =
                sqlx::query_as("SELECT id, name::text, color FROM labels WHERE repo_id = $1 AND name = $2::citext")
                    .bind(repo)
                    .bind(n)
                    .fetch_optional(&mut **tx)
                    .await?;
            match row {
                Some(r) => {
                    if !out.iter().any(|(id, _, _)| *id == r.0) {
                        out.push(r);
                    }
                }
                None => {
                    return Err(Error::Validation(vec![FieldError::custom(
                        "Issue",
                        "labels",
                        format!("label \"{n}\" does not exist in this repository"),
                    )]));
                }
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------ milestones

    pub async fn milestones(&self, actor: &Actor, repo: RepoId, open: Option<bool>) -> Result<Vec<Milestone>> {
        self.need_read(actor, repo).await?;
        let sql = format!(
            "{MILESTONE_SELECT} WHERE m.repo_id = $1 AND ($2::bool IS NULL OR m.open = $2) GROUP BY m.id ORDER BY m.due_on ASC NULLS LAST, m.number ASC"
        );
        let rows = sqlx::query_as::<_, MilestoneRow>(&sql).bind(repo).bind(open).fetch_all(self.db()).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn milestone(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Milestone> {
        self.need_read(actor, repo).await?;
        self.milestone_by_number(repo, number).await
    }

    pub async fn milestone_by_number(&self, repo: RepoId, number: i64) -> Result<Milestone> {
        let sql = format!("{MILESTONE_SELECT} WHERE m.repo_id = $1 AND m.number = $2 GROUP BY m.id");
        sqlx::query_as::<_, MilestoneRow>(&sql)
            .bind(repo)
            .bind(number)
            .fetch_optional(self.db())
            .await?
            .map(Into::into)
            .ok_or(Error::NotFound)
    }

    pub async fn create_milestone(&self, actor: &Actor, repo: RepoId, draft: MilestoneDraft, _client: &ClientInfo) -> Result<Milestone> {
        self.need(actor, repo, Permission::Triage).await?;
        let title = util::title("Milestone", "title", &draft.title)?;
        let description = util::body("Milestone", "description", &draft.description.unwrap_or_default())?;
        let open = draft.open.unwrap_or(true);
        let mut tx = self.db().begin().await?;
        // The repository row is the lock that serialises numbering, like for issues.
        sqlx::query("SELECT id FROM repositories WHERE id = $1 FOR UPDATE").bind(repo).fetch_one(&mut *tx).await?;
        let number: i64 = sqlx::query_scalar("SELECT COALESCE(max(number), 0) + 1 FROM milestones WHERE repo_id = $1")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
        let closed_at = if open { None } else { Some(Utc::now()) };
        let clash: Option<i32> = sqlx::query_scalar("SELECT 1 FROM milestones WHERE repo_id = $1 AND lower(title) = lower($2)")
            .bind(repo)
            .bind(&title)
            .fetch_optional(&mut *tx)
            .await?;
        if clash.is_some() {
            return Err(Error::Validation(vec![FieldError::new("Milestone", "title", "already_exists")]));
        }
        let res = sqlx::query(
            "INSERT INTO milestones (repo_id, number, title, description, open, due_on, closed_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(repo)
        .bind(number)
        .bind(&title)
        .bind(&description)
        .bind(open)
        .bind(draft.due_on)
        .bind(closed_at)
        .execute(&mut *tx)
        .await;
        match res {
            Ok(_) => {}
            Err(e) if util::is_unique(&e) => return Err(Error::Validation(vec![FieldError::new("Milestone", "title", "already_exists")])),
            Err(e) => return Err(e.into()),
        }
        tx.commit().await?;
        self.milestone_by_number(repo, number).await
    }

    pub async fn update_milestone(
        &self,
        actor: &Actor,
        repo: RepoId,
        number: i64,
        edit: MilestoneEdit,
        _client: &ClientInfo,
    ) -> Result<Milestone> {
        self.need(actor, repo, Permission::Triage).await?;
        let cur = self.milestone_by_number(repo, number).await?;
        let title = match &edit.title {
            Some(t) => util::title("Milestone", "title", t)?,
            None => cur.title.clone(),
        };
        let description = match &edit.description {
            Some(d) => util::body("Milestone", "description", d)?,
            None => cur.description.clone(),
        };
        let due_on = match edit.due_on {
            Some(v) => v,
            None => cur.due_on,
        };
        let open = edit.open.unwrap_or(cur.open);
        let closed_at = match (cur.open, open) {
            (true, false) => Some(Utc::now()),
            (false, true) => None,
            _ => cur.closed_at,
        };
        let clash: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM milestones WHERE repo_id = $1 AND lower(title) = lower($2) AND id <> $3")
                .bind(repo)
                .bind(&title)
                .bind(cur.id)
                .fetch_optional(self.db())
                .await?;
        if clash.is_some() {
            return Err(Error::Validation(vec![FieldError::new("Milestone", "title", "already_exists")]));
        }
        let res = sqlx::query("UPDATE milestones SET title = $1, description = $2, due_on = $3, open = $4, closed_at = $5 WHERE id = $6")
            .bind(&title)
            .bind(&description)
            .bind(due_on)
            .bind(open)
            .bind(closed_at)
            .bind(cur.id)
            .execute(self.db())
            .await;
        match res {
            Ok(_) => {}
            Err(e) if util::is_unique(&e) => return Err(Error::Validation(vec![FieldError::new("Milestone", "title", "already_exists")])),
            Err(e) => return Err(e.into()),
        }
        self.milestone_by_number(repo, number).await
    }

    pub async fn delete_milestone(&self, actor: &Actor, repo: RepoId, number: i64, _client: &ClientInfo) -> Result<()> {
        self.need(actor, repo, Permission::Triage).await?;
        let cur = self.milestone_by_number(repo, number).await?;
        sqlx::query("DELETE FROM milestones WHERE id = $1").bind(cur.id).execute(self.db()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_labels_are_recognised_case_insensitively() {
        assert!(is_default_label("Bug", ""));
        assert!(is_default_label("good first issue", ""));
        assert!(!is_default_label("priority", ""));
    }

    #[test]
    fn label_names_are_checked() {
        assert_eq!(label_name("  bug ").unwrap(), "bug");
        assert!(label_name("").is_err());
        assert!(label_name("a,b").is_err());
        assert!(label_name(&"x".repeat(51)).is_err());
    }

    #[test]
    fn default_colors_are_valid() {
        for (_, c, _) in DEFAULT_LABELS {
            assert!(util::color(c).is_ok());
        }
    }
}
