//! What every full page shares: title, assets, the signed in user, the flash message, the
//! repository sidebar. Templates read it as `c`.

use crate::assets;
use ch_core::Permission;
use ch_core::model::Repo;

#[derive(Debug, Clone, Default)]
pub struct NavUser {
    pub username: String,
    pub display_name: String,
    /// A site admin with two-factor on, who may open the admin pages.
    pub is_admin: bool,
    /// An admin who has not turned two-factor on yet.
    pub admin_needs_2fa: bool,
    pub unread: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Flash {
    /// `ok`, `error` or `info`.
    pub kind: String,
    pub text: String,
}

/// The repository header and sidebar.
#[derive(Debug, Clone, Default)]
pub struct RepoNav {
    pub owner: String,
    pub name: String,
    pub base: String,
    pub description: String,
    pub private: bool,
    /// The chip: `Private`, `Internal` or `Public`, and its explanation.
    pub tier: &'static str,
    pub tier_title: &'static str,
    pub archived: bool,
    pub is_empty: bool,
    pub default_branch: String,
    /// The sidebar entry that is current: `code`, `issues`, `pulls`, `actions`, `releases`, `settings`.
    pub section: String,
    pub can_write: bool,
    pub can_admin: bool,
    pub can_maintain: bool,
    pub can_triage: bool,
    /// The workspace URL for the header's "Open workspace" button, or empty when the button
    /// must not show (signed out, a public read, or the editor off).
    pub workspace: String,
    /// Watch, star and fork buttons, for a signed-in viewer only.
    pub social: Option<SocialNav>,
}

/// The header's watch, star and fork buttons.
#[derive(Debug, Clone, Default)]
pub struct SocialNav {
    /// `participating`, `all` or `ignore`.
    pub watch: String,
    pub stars: i64,
    pub starred: bool,
    pub forks: i64,
}

impl RepoNav {
    pub fn new(repo: &Repo, perm: Permission, section: &str) -> Self {
        RepoNav {
            owner: repo.owner_name.clone(),
            name: repo.name.clone(),
            base: format!("/{}/{}", crate::fmt::enc(&repo.owner_name), crate::fmt::enc(&repo.name)),
            description: repo.description.clone(),
            private: repo.visibility == ch_core::model::Visibility::Private,
            tier: crate::views::visibility::chip(repo.visibility).0,
            tier_title: crate::views::visibility::chip(repo.visibility).1,
            archived: repo.archived,
            is_empty: repo.is_empty,
            default_branch: repo.default_branch.clone(),
            section: section.into(),
            can_write: perm.allows(Permission::Write) && !repo.archived,
            can_admin: perm.allows(Permission::Admin),
            can_maintain: perm.allows(Permission::Maintain),
            can_triage: perm.allows(Permission::Triage) && !repo.archived,
            workspace: String::new(),
            social: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Chrome {
    pub title: String,
    pub instance: String,
    /// `system`, `light` or `dark`.
    pub theme: String,
    pub css_app: String,
    pub css_code: String,
    pub css_forge: String,
    pub js_app: String,
    pub js_htmx: String,
    pub js_sse: String,
    pub icons: String,
    pub user: Option<NavUser>,
    pub csrf: String,
    pub flash: Option<Flash>,
    pub repo: Option<RepoNav>,
    /// Which header link is current: `home`, `explore`, `notifications`, `settings`, `admin`.
    pub nav: String,
    /// A line shown above the page, such as "turn on two-factor".
    pub banner: Option<String>,
    /// A signed-out visitor reading a public repository: show the sign-in prompt, no forms.
    pub signed_out_reader: bool,
    /// The documentation address from `server.docs_url`; empty means no Docs link.
    pub docs_url: String,
}

impl Chrome {
    pub fn base(title: &str, instance: &str, theme: &str) -> Self {
        Chrome {
            title: if title.is_empty() { instance.to_string() } else { format!("{title} - {instance}") },
            instance: instance.into(),
            theme: theme.into(),
            css_app: assets::url("app.css"),
            css_code: assets::url("code.css"),
            css_forge: assets::url("forge.css"),
            js_app: assets::url("app.js"),
            js_htmx: assets::url("htmx.min.js"),
            js_sse: assets::url("htmx-sse.js"),
            icons: assets::url("icons.svg"),
            ..Default::default()
        }
    }

    pub fn repo(mut self, nav: RepoNav) -> Self {
        self.repo = Some(nav);
        self
    }

    pub fn nav(mut self, nav: &str) -> Self {
        self.nav = nav.into();
        self
    }
}
