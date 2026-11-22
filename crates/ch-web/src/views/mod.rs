//! One module per area of the site. Handlers load data through the service traits, shape it
//! into plain view structs, and hand them to a template.

use crate::fmt;
use ch_core::model::User;
use ch_core::state::AppState;
use ch_core::web_contract::Label;
use ch_core::{Error, UserId};
use std::collections::HashMap;

/// A query string as a map.
pub type Params = HashMap<String, String>;
pub use axum::extract::Query;

pub mod actions;
pub mod admin;
pub mod apps;
pub mod auth;
pub mod code;
pub mod home;
pub mod issues;
pub mod people;
pub mod pulls;
pub mod releases;
pub mod repo;
pub mod repo_common;
pub mod repo_settings;
pub mod settings;

/// A label as the templates draw it.
#[derive(Debug, Clone, Default)]
pub struct LabelView {
    pub name: String,
    pub class: String,
    pub href: String,
    pub description: String,
}

impl LabelView {
    /// `base` is the issues list the label links to, such as `/alice/demo/issues`.
    pub fn new(name: &str, color: &str, description: &str, base: &str) -> Self {
        LabelView {
            name: name.into(),
            class: fmt::label_class(color),
            href: format!("{base}?labels={}", fmt::enc(name)),
            description: description.into(),
        }
    }

    pub fn from_label(l: &Label, base: &str) -> Self {
        Self::new(&l.name, &l.color, &l.description, base)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Pager {
    pub prev: Option<String>,
    pub next: Option<String>,
}

impl Pager {
    /// Page links for a list where we fetched `per_page + 1` rows to know whether more exist.
    pub fn new(base: &str, page: u32, has_more: bool) -> Self {
        let sep = if base.contains('?') { '&' } else { '?' };
        Pager {
            prev: if page > 1 { Some(if page == 2 { base.to_string() } else { format!("{base}{sep}page={}", page - 1) }) } else { None },
            next: if has_more { Some(format!("{base}{sep}page={}", page + 1)) } else { None },
        }
    }
}

/// A person as a link: username for the URL and avatar, display name for reading.
#[derive(Debug, Clone, Default)]
pub struct UserRef {
    pub username: String,
    pub display: String,
}

impl UserRef {
    pub fn ghost() -> Self {
        UserRef { username: "ghost".into(), display: "Deleted user".into() }
    }
    pub fn from_user(u: &User) -> Self {
        UserRef {
            username: u.username.clone(),
            display: if u.display_name.is_empty() { u.username.clone() } else { u.display_name.clone() },
        }
    }
}

/// Looks people up once per request.
pub struct People {
    state: AppState,
    seen: HashMap<UserId, UserRef>,
}

impl People {
    pub fn new(state: &AppState) -> Self {
        People { state: state.clone(), seen: HashMap::new() }
    }

    pub async fn get(&mut self, id: UserId) -> UserRef {
        if let Some(u) = self.seen.get(&id) {
            return u.clone();
        }
        let u = match self.state.accounts.get_user(id).await {
            Ok(u) => UserRef::from_user(&u),
            Err(_) => UserRef::ghost(),
        };
        self.seen.insert(id, u.clone());
        u
    }
}

/// Field errors from a failed service call, keyed by field name.
#[derive(Debug, Clone, Default)]
pub struct FieldErrors {
    pub general: String,
    map: HashMap<String, String>,
}

impl FieldErrors {
    pub fn from_error(e: &Error) -> Option<Self> {
        match e {
            Error::Validation(list) => {
                let mut out = FieldErrors::default();
                for f in list {
                    let msg = f.message.clone().unwrap_or_else(|| default_message(&f.field, &f.code));
                    out.map.entry(f.field.clone()).or_insert(msg);
                }
                Some(out)
            }
            Error::Conflict(m) => Some(FieldErrors { general: m.clone(), map: HashMap::new() }),
            _ => None,
        }
    }

    pub fn general(msg: &str) -> Self {
        FieldErrors { general: msg.into(), map: HashMap::new() }
    }

    pub fn with(mut self, field: &str, msg: &str) -> Self {
        self.map.insert(field.into(), msg.into());
        self
    }

    pub fn get(&self, field: &str) -> String {
        self.map.get(field).cloned().unwrap_or_default()
    }

    pub fn any(&self) -> bool {
        !self.general.is_empty() || !self.map.is_empty()
    }
}

fn default_message(field: &str, code: &str) -> String {
    match code {
        "missing" | "missing_field" => format!("Please fill in {field}."),
        "already_exists" | "taken" => format!("That {field} is already taken."),
        "invalid" => format!("That {field} is not valid."),
        "too_short" => format!("That {field} is too short."),
        "too_weak" | "weak" => "Choose a password that is harder to guess.".to_string(),
        _ => format!("Check {field}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_core::FieldError;

    #[test]
    fn pager_links() {
        let p = Pager::new("/a/b/issues?state=open", 1, true);
        assert_eq!(p.prev, None);
        assert_eq!(p.next.as_deref(), Some("/a/b/issues?state=open&page=2"));
        let p = Pager::new("/a/b/commits", 3, false);
        assert_eq!(p.prev.as_deref(), Some("/a/b/commits?page=2"));
        assert_eq!(p.next, None);
        let p = Pager::new("/a/b/commits", 2, true);
        assert_eq!(p.prev.as_deref(), Some("/a/b/commits"));
    }

    #[test]
    fn field_errors_map_codes_to_words() {
        let e = Error::Validation(vec![FieldError::new("user", "username", "taken"), FieldError::custom("user", "password", "Too short.")]);
        let f = FieldErrors::from_error(&e).unwrap();
        assert_eq!(f.get("username"), "That username is already taken.");
        assert_eq!(f.get("password"), "Too short.");
        assert!(f.any());
        assert!(FieldErrors::from_error(&Error::NotFound).is_none());
    }

    #[test]
    fn label_view_links_to_the_filtered_list() {
        let l = LabelView::new("good first issue", "0e8a16", "", "/a/b/issues");
        assert_eq!(l.href, "/a/b/issues?labels=good%20first%20issue");
        assert_eq!(l.class, "lbl-4");
    }
}
