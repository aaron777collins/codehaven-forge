//! Small helpers: input checks, paging and time.

use ch_core::{Actor, AuthVia, Error, FieldError, RepoId, Result, UserId};

pub const MAX_TITLE: usize = 256;
pub const MAX_BODY: usize = 65_536;
pub const MAX_PER_PAGE: u32 = 100;
pub const DEFAULT_PER_PAGE: u32 = 30;

/// The account behind the caller. Jobs, runners and `System` have none, and none of them may
/// open issues, review or merge.
pub fn acting_user(actor: &Actor) -> Result<UserId> {
    actor.user_id().ok_or_else(|| Error::forbidden("This action needs a signed-in account"))
}

/// The person behind a call that is already limited to what the actor can see, such as which
/// issues a filter's `@me` means. Includes a connected app, which `acting_user` does not.
pub fn author_user(actor: &Actor) -> Result<UserId> {
    actor.author_id().ok_or_else(|| Error::forbidden("This action needs a signed-in account"))
}

/// The account behind a caller who is about to change something. A personal access token
/// without the `repo` scope is read-only even though reading is all a reader may do here:
/// opening an issue, commenting and reacting are writes, and `Authz` caps such a token at read.
pub fn writing_user(actor: &Actor, repo: RepoId) -> Result<UserId> {
    // A connected app writes only where its grant says so. Issues and comments need only Read from
    // `Authz`, so without this an app with write on one repository could write to every other
    // repository it can merely read.
    if let Actor::App { user_id, grant, .. } = actor {
        if !grant.write.has(repo) {
            return Err(Error::forbidden("This connection was not given write access to this repository"));
        }
        return Ok(*user_id);
    }
    let user = acting_user(actor)?;
    if let Actor::Token { scopes, .. } = actor
        && !scopes.has("repo")
    {
        return Err(Error::forbidden("This token is read-only. It needs the repo scope to make changes"));
    }
    Ok(user)
}

/// An `Actor` for another account, to ask `Authz` what that account may do. It never carries
/// site admin power, so it only answers questions about the repository.
pub fn as_user(id: UserId) -> Actor {
    Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

pub fn title(resource: &str, field: &str, s: &str) -> Result<String> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Error::Validation(vec![FieldError::new(resource, field, "missing_field")]));
    }
    if t.chars().count() > MAX_TITLE {
        return Err(Error::invalid(resource, field, format!("{field} is too long (maximum is {MAX_TITLE} characters)")));
    }
    if t.chars().any(|c| c.is_control()) {
        return Err(Error::invalid(resource, field, format!("{field} must be a single line of plain text")));
    }
    Ok(t.to_string())
}

pub fn body(resource: &str, field: &str, s: &str) -> Result<String> {
    if s.contains('\0') {
        return Err(Error::invalid(resource, field, format!("{field} cannot contain null bytes")));
    }
    if s.chars().count() > MAX_BODY {
        return Err(Error::invalid(resource, field, format!("{field} is too long (maximum is {MAX_BODY} characters)")));
    }
    Ok(s.replace("\r\n", "\n"))
}

pub fn required_body(resource: &str, field: &str, s: &str) -> Result<String> {
    let b = body(resource, field, s)?;
    if b.trim().is_empty() {
        return Err(Error::Validation(vec![FieldError::new(resource, field, "missing_field")]));
    }
    Ok(b)
}

/// 1-based page and a clamped page size.
pub fn page(page: u32, per_page: u32) -> (i64, i64) {
    let page = page.max(1) as i64;
    let per = if per_page == 0 { DEFAULT_PER_PAGE } else { per_page.min(MAX_PER_PAGE) } as i64;
    ((page - 1) * per, per)
}

/// A color is six hex digits, with or without `#`. Stored lowercase without `#`.
pub fn color(s: &str) -> Result<String> {
    let c = s.trim().trim_start_matches('#');
    if c.len() == 6 && c.chars().all(|x| x.is_ascii_hexdigit()) {
        Ok(c.to_ascii_lowercase())
    } else {
        Err(Error::invalid("Label", "color", "color must be six hex digits like 0f766e"))
    }
}

/// Escapes `%`, `_` and `\` so user text can sit inside an `ILIKE` pattern.
pub fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn not_found<T>() -> Result<T> {
    Err(Error::NotFound)
}

/// True when a database error is a unique constraint violation.
pub fn is_unique(e: &sqlx::Error) -> bool {
    e.as_database_error().is_some_and(|d| d.is_unique_violation())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_tokens_cannot_write() {
        use ch_core::{Scopes, TokenId};
        let tok = |scopes: &[&str]| Actor::Token { user_id: UserId(1), token_id: TokenId(1), scopes: Scopes::new(scopes.iter().copied()) };
        assert!(writing_user(&tok(&["repo"]), RepoId(1)).is_ok());
        assert!(matches!(writing_user(&tok(&["repo:read"]), RepoId(1)), Err(Error::Forbidden(_))));
        assert!(acting_user(&tok(&["repo:read"])).is_ok(), "reading is fine");
        assert!(writing_user(&as_user(UserId(2)), RepoId(1)).is_ok());
        assert!(writing_user(&Actor::Anonymous, RepoId(1)).is_err());
        assert!(writing_user(&Actor::System, RepoId(1)).is_err());
    }

    #[test]
    fn titles_are_trimmed_and_checked() {
        assert_eq!(title("Issue", "title", "  hello ").unwrap(), "hello");
        assert!(title("Issue", "title", "   ").is_err());
        assert!(title("Issue", "title", &"x".repeat(257)).is_err());
        assert!(title("Issue", "title", "a\nb").is_err());
        assert!(title("Issue", "title", &"é".repeat(256)).is_ok());
    }

    #[test]
    fn bodies_normalise_line_endings() {
        assert_eq!(body("Issue", "body", "a\r\nb").unwrap(), "a\nb");
        assert!(body("Issue", "body", &"x".repeat(MAX_BODY + 1)).is_err());
        assert!(required_body("Comment", "body", " \n").is_err());
    }

    #[test]
    fn paging_clamps() {
        assert_eq!(page(0, 0), (0, 30));
        assert_eq!(page(2, 10), (10, 10));
        assert_eq!(page(1, 1000), (0, 100));
    }

    #[test]
    fn colors() {
        assert_eq!(color("#D73A4A").unwrap(), "d73a4a");
        assert!(color("zzzzzz").is_err());
        assert!(color("fff").is_err());
    }

    #[test]
    fn like_escapes() {
        assert_eq!(like_escape("50%_\\"), "50\\%\\_\\\\");
    }
}
