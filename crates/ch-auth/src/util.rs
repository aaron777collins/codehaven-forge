//! Small shared helpers: name and email rules, database error mapping, time arithmetic.

use ch_core::error::{Error, FieldError};

/// Names that would collide with a route or confuse people. Compared lower case.
const RESERVED: &[&str] = &[
    "admin",
    "administrator",
    "api",
    "assets",
    "avatar",
    "about",
    "blog",
    "codehaven",
    "dashboard",
    "explore",
    "favicon",
    "git",
    "healthz",
    "help",
    "invite",
    "issues",
    "login",
    "logout",
    "new",
    "notifications",
    "orgs",
    "preview",
    "organizations",
    "password",
    "pulls",
    "readyz",
    "repos",
    "robots",
    "root",
    "search",
    "settings",
    "signin",
    "signup",
    "site-admin",
    "static",
    "support",
    "system",
    "user",
    "users",
    "v2",
    "metrics",
    "me",
    "mcp",
    "oauth",
    "none",
    "null",
    // The package pages live at /packages and /nuget (ch-2fc).
    "packages",
    "nuget",
    "undefined",
];

pub fn is_reserved(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    RESERVED.contains(&lower.as_str())
}

/// The same pattern as the `users.username` CHECK, plus the reserved names.
pub fn validate_username(name: &str, resource: &str) -> Result<(), Error> {
    let b = name.as_bytes();
    let ok_len = !b.is_empty() && b.len() <= 39;
    let ok_chars = b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-');
    let ok_edges = ok_len && b[0].is_ascii_alphanumeric() && b[b.len() - 1].is_ascii_alphanumeric();
    let ok_hyphens = !name.contains("--");
    if !(ok_len && ok_chars && ok_edges && ok_hyphens) {
        return Err(Error::Validation(vec![FieldError::custom(
            resource,
            "username",
            "Use 1 to 39 letters, numbers and single hyphens, starting and ending with a letter or number.",
        )]));
    }
    if is_reserved(name) {
        return Err(Error::Validation(vec![FieldError::custom(resource, "username", "That name is reserved.")]));
    }
    Ok(())
}

/// A plain sanity check, not a full RFC 5322 parser: one `@`, something on both sides, no
/// spaces or control characters, a dot in the domain.
pub fn validate_email(email: &str) -> Result<(), Error> {
    let bad = || Error::Validation(vec![FieldError::custom("Email", "email", "That does not look like an email address.")]);
    if email.len() > 254 || email.chars().any(|c| c.is_whitespace() || c.is_control() || c == ',' || c == ';' || c == '<' || c == '>') {
        return Err(bad());
    }
    let Some((local, domain)) = email.split_once('@') else { return Err(bad()) };
    if local.is_empty() || local.len() > 64 || domain.len() < 3 || domain.contains('@') || !domain.contains('.') {
        return Err(bad());
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.starts_with('-') || domain.contains("..") {
        return Err(bad());
    }
    Ok(())
}

pub fn normalize_email(email: &str) -> String {
    email.trim().to_string()
}

/// True when a database error is a unique violation, optionally on a named constraint.
pub fn is_unique_violation(e: &sqlx::Error, constraint_contains: &str) -> bool {
    match e.as_database_error() {
        Some(d) => {
            d.code().as_deref() == Some("23505")
                && (constraint_contains.is_empty() || d.constraint().is_some_and(|c| c.contains(constraint_contains)))
        }
        None => false,
    }
}

pub fn generic_credentials_error() -> Error {
    Error::Unauthorized("That username and password don't match.".into())
}

/// Truncates user-agent strings and the like for storage.
pub fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        assert!(validate_username("alice", "User").is_ok());
        assert!(validate_username("a", "User").is_ok());
        assert!(validate_username("a-b", "User").is_ok());
        assert!(validate_username("a-b-c", "User").is_ok());
        assert!(validate_username("a--b", "User").is_err(), "double hyphen");
        assert!(validate_username("-a", "User").is_err());
        assert!(validate_username("a-", "User").is_err());
        assert!(validate_username("a_b", "User").is_err());
        assert!(validate_username("a b", "User").is_err());
        assert!(validate_username("", "User").is_err());
        assert!(validate_username(&"a".repeat(40), "User").is_err());
        assert!(validate_username("Admin", "User").is_err());
        assert!(validate_username("login", "User").is_err());
        assert!(validate_username("名前", "User").is_err());
    }

    #[test]
    fn emails() {
        assert!(validate_email("a@b.co").is_ok());
        assert!(validate_email("a+tag@sub.example.org").is_ok());
        assert!(validate_email("a@b").is_err());
        assert!(validate_email("@b.co").is_err());
        assert!(validate_email("a b@c.co").is_err());
        assert!(validate_email("a@@b.co").is_err());
        assert!(validate_email("a@b..co").is_err());
        assert!(validate_email("a@b.co,c@d.co").is_err());
        assert!(validate_email(&format!("{}@b.co", "a".repeat(70))).is_err());
    }
}
