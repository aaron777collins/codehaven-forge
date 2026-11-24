//! What the service, the host router and the publisher share: the handles, the keys, and the
//! one rule for who may see a site.

use ch_core::model::Visibility;
use ch_core::pages::serving_tier;
use ch_core::services::{Audit, Authz, BlobStore, EventBus, GitService, Repos};
use ch_core::state::AppStateBuilder;
use ch_core::{Actor, AuthVia, Config, Permission, RepoId, Result, UserId};
use rand::Rng;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};

/// Content requests being looked up at once. Past it a request gets 503.
pub const AT_ONCE: usize = 64;
/// Replies whose body is still being sent. Each holds at most a small piece of its file, and
/// past this a request gets 503, so clients that never read cannot grow the server's memory.
/// A reply that stalls or runs past its deadline gives its place back, and one client address
/// holds at most `pages.replies_per_address` of them, so a few clients cannot hold them all.
pub const OPEN_REPLIES: usize = 512;
/// How long a reply waits for one of its address's places before 503, and how often it looks.
pub const ADDRESS_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
const ADDRESS_POLL: std::time::Duration = std::time::Duration::from_millis(20);
/// Characters of bridge storage per project.
pub const STORAGE_QUOTA: usize = 1_000_000;
/// Ready copies kept besides the live one.
pub const KEEP_READY: i64 = 5;
/// Failed copies kept, so the settings page can say why.
pub const KEEP_FAILED: i64 = 5;

pub struct Core {
    pub config: Arc<Config>,
    pub db: PgPool,
    pub git: Arc<dyn GitService>,
    pub authz: Arc<dyn Authz>,
    pub repos: Arc<dyn Repos>,
    pub blobs: Arc<dyn BlobStore>,
    pub audit: Arc<dyn Audit>,
    pub events: Arc<dyn EventBus>,
    /// The pages host, lowercase. `None`: Pages is off.
    pub host: Option<String>,
    pub key: [u8; 32],
    pub busy: Arc<Semaphore>,
    /// One place per reply until its body is sent, dropped, stalls or runs past its deadline.
    pub open: Arc<Semaphore>,
    /// Replies open per client address, until each body is dropped.
    pub addresses: Arc<Mutex<HashMap<String, usize>>>,
    /// Wakes the publisher when a copy is queued.
    pub wake: Notify,
    full: Mutex<Option<(Instant, bool)>>,
}

/// A repository's site as the host router needs it.
#[derive(Debug, Clone)]
pub struct Site {
    pub repo: RepoId,
    pub owner: String,
    pub name: String,
    pub repo_tier: Visibility,
    pub audience: Visibility,
    /// The live copy and the tier it was made at.
    pub live: Option<(i64, Visibility)>,
}

/// One of the replies a client address may have open. Given back when dropped.
pub struct AddressSlot {
    map: Arc<Mutex<HashMap<String, usize>>>,
    key: String,
}

impl Drop for AddressSlot {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

impl Core {
    /// A slot for one more open reply from `key`, or `None` when it has its share already.
    pub fn take_address(&self, key: &str) -> Option<AddressSlot> {
        let cap = self.config.pages.replies_per_address.max(1) as usize;
        let mut map = self.addresses.lock().unwrap_or_else(|e| e.into_inner());
        let n = map.entry(key.to_string()).or_insert(0);
        if *n >= cap {
            return None;
        }
        *n += 1;
        Some(AddressSlot { map: self.addresses.clone(), key: key.to_string() })
    }

    /// `take_address`, waiting up to `ADDRESS_WAIT` for one of `key`'s own replies to finish.
    /// A page that asks for many small files at once then gets them all, while an address that
    /// holds its share open still gets 503. Nothing else is held while it waits.
    pub async fn take_address_soon(&self, key: &str) -> Option<AddressSlot> {
        let until = tokio::time::Instant::now() + ADDRESS_WAIT;
        loop {
            if let Some(slot) = self.take_address(key) {
                return Some(slot);
            }
            if tokio::time::Instant::now() >= until {
                return None;
            }
            tokio::time::sleep(ADDRESS_POLL).await;
        }
    }

    pub fn from_builder(b: &AppStateBuilder) -> Self {
        let mut master = [0u8; 32];
        match std::fs::read(b.config.master_key_path()) {
            Ok(bytes) if bytes.len() == 32 => master.copy_from_slice(&bytes),
            // No key file (a test): tokens then last only as long as this process.
            _ => rand::rng().fill(&mut master),
        }
        Self {
            config: b.config.clone(),
            db: b.db.clone(),
            git: b.git.clone(),
            authz: b.authz.clone(),
            repos: b.repos.clone(),
            blobs: b.blobs.clone(),
            audit: b.audit.clone(),
            events: b.events.clone(),
            host: b.config.pages.host(),
            key: crate::token::derive_key(&master),
            busy: Arc::new(Semaphore::new(AT_ONCE)),
            open: Arc::new(Semaphore::new(OPEN_REPLIES)),
            addresses: Arc::new(Mutex::new(HashMap::new())),
            wake: Notify::new(),
            full: Mutex::new(None),
        }
    }

    /// The app's scheme, and its port when it names one, so the pages host matches it.
    fn scheme_port(&self) -> (String, String) {
        let url = self.config.external_url_parts().ok();
        let scheme = url.as_ref().map(|u| u.scheme.clone()).unwrap_or_else(|| "https".into());
        let port = url.and_then(|u| u.host.rsplit_once(':').map(|(_, p)| format!(":{p}"))).unwrap_or_default();
        (scheme, port)
    }

    /// `https://pages.example.com`.
    pub fn origin(&self) -> String {
        let (scheme, port) = self.scheme_port();
        format!("{scheme}://{}{port}", self.host.as_deref().unwrap_or(""))
    }

    /// `https://<owner>.pages.example.com`, lowercase.
    pub fn owner_origin(&self, owner: &str) -> String {
        let (scheme, port) = self.scheme_port();
        format!("{scheme}://{}.{}{port}", owner.to_ascii_lowercase(), self.host.as_deref().unwrap_or(""))
    }

    /// Where people open a site.
    pub fn site_url(&self, owner: &str, project: &str, full: bool) -> String {
        if full && ch_core::pages::valid_owner_label(owner) {
            format!("{}/{project}/", self.owner_origin(owner))
        } else {
            format!("{}/{owner}/{project}/", self.origin())
        }
    }

    /// The app route that makes a token: signed out it is the standard sign-in redirect.
    pub fn open_url(&self, owner: &str, project: &str) -> String {
        format!("{}/{owner}/{project}/pages/open?path=", self.config.origin())
    }

    /// Full mode, read at most every five seconds.
    pub async fn full_mode(&self) -> bool {
        if let Ok(g) = self.full.lock()
            && let Some((at, v)) = *g
            && at.elapsed() < Duration::from_secs(5)
        {
            return v;
        }
        let v: bool =
            sqlx::query_scalar("SELECT full_mode FROM pages_settings").fetch_optional(&self.db).await.ok().flatten().unwrap_or(false);
        if let Ok(mut g) = self.full.lock() {
            *g = Some((Instant::now(), v));
        }
        v
    }

    pub fn forget_full_mode(&self) {
        if let Ok(mut g) = self.full.lock() {
            *g = None;
        }
    }

    /// The site named by an owner and a project, by the names as written (case does not
    /// matter, as everywhere in Codehaven). `None` when there is none.
    pub async fn find_site(&self, owner: &str, project: &str) -> Result<Option<Site>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            id: i64,
            owner: String,
            name: String,
            access_tier: String,
            audience: String,
            dep: Option<i64>,
            dep_tier: Option<String>,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT r.id, o.username::text AS owner, r.name::text AS name, r.access_tier, s.audience, d.id AS dep, d.tier AS dep_tier \
             FROM repositories r JOIN users o ON o.id = r.owner_id \
             JOIN pages_sites s ON s.repo_id = r.id \
             LEFT JOIN pages_deployments d ON d.id = s.live_id AND d.state = 'ready' \
             WHERE o.username = $1::citext AND r.name = $2::citext",
        )
        .bind(owner)
        .bind(project)
        .fetch_optional(&self.db)
        .await?;
        let Some(r) = row else { return Ok(None) };
        // An unknown stored tier reads as private.
        let tier = |s: &str| Visibility::parse(s).unwrap_or(Visibility::Private);
        Ok(Some(Site {
            repo: RepoId(r.id),
            owner: r.owner,
            name: r.name,
            repo_tier: tier(&r.access_tier),
            audience: tier(&r.audience),
            live: match (r.dep, r.dep_tier) {
                (Some(id), Some(t)) => Some((id, tier(&t))),
                _ => None,
            },
        }))
    }

    /// The one rule (design section 5): the site has a live copy, and the viewer is covered by
    /// the lowest of the audience, the copy's tier and the repository's tier now. Signed out
    /// needs the repository readable to anyone right now; signed in, internal needs an active
    /// account that reads the repository, and private needs a grant. Always checked live.
    pub async fn may_view(&self, site: &Site, viewer: Option<UserId>) -> Result<bool> {
        let Some((_, copy_tier)) = site.live else { return Ok(false) };
        let tier = serving_tier(site.audience, copy_tier, site.repo_tier);
        if tier == Visibility::Public && self.authz.repo_permission(&Actor::Anonymous, site.repo).await? >= Permission::View {
            return Ok(true);
        }
        let Some(user) = viewer else { return Ok(false) };
        let actor = viewer_actor(user);
        Ok(match tier {
            Visibility::Public | Visibility::Internal => self.authz.repo_permission(&actor, site.repo).await? >= Permission::Read,
            Visibility::Private => self.authz.has_grant(&actor, site.repo).await?,
        })
    }
}

/// The viewer a token names, with no power beyond their own grants: never site admin, so a
/// token can never do more than the person's ordinary access.
pub fn viewer_actor(user: UserId) -> Actor {
    Actor::User { id: user, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

/// A path inside a site for a redirect: starts with `/`, keeps an optional query, drops a
/// fragment, and percent-encodes every byte outside the characters a URL path or query may
/// hold as they are, so nothing in it can end the address or start another one.
pub fn safe_inner_path(p: &str) -> String {
    let p = p.split('#').next().unwrap_or("");
    let mut out = String::from("/");
    let body = p.trim_start_matches('/');
    for b in body.bytes() {
        let keep = b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/%?".contains(&b);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inner_paths_stay_inside() {
        assert_eq!(safe_inner_path("/guide/a.html?x=1#top"), "/guide/a.html?x=1");
        assert_eq!(safe_inner_path(""), "/");
        assert_eq!(safe_inner_path("//evil.example/x"), "/evil.example/x");
        assert_eq!(safe_inner_path("/a b\r\nSet-Cookie: x"), "/a%20b%0D%0ASet-Cookie:%20x");
        assert_eq!(safe_inner_path("/<script>\"'"), "/%3Cscript%3E%22'");
        assert_eq!(safe_inner_path("/é"), "/%C3%A9");
    }
}
