//! The address put on commits the server makes for a person, so no account email ever lands
//! in git history.

/// `<id>+<username>@users.noreply.<host>`. `external_url` is the instance origin such as
/// `https://code.example.com:8443`; the scheme and any port are dropped. An empty or
/// unparseable origin gives the host `codehaven.invalid`.
pub fn noreply_email(id: i64, username: &str, external_url: &str) -> String {
    format!("{id}+{username}@users.noreply.{}", host_of(external_url))
}

/// The account id and lower-cased username inside a noreply address made by `noreply_email`
/// for this host. The caller checks that the username still belongs to that id.
pub fn parse_noreply(email: &str, external_url: &str) -> Option<(i64, String)> {
    let lower = email.to_lowercase();
    let suffix = format!("@users.noreply.{}", host_of(external_url).to_lowercase());
    let local = lower.strip_suffix(&suffix)?;
    let (digits, rest) = local.split_once('+')?;
    if digits.is_empty() || rest.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok().map(|id| (id, rest.to_string()))
}

/// The host of an instance origin: scheme and port dropped, `codehaven.invalid` when empty.
fn host_of(external_url: &str) -> &str {
    let no_scheme = external_url.strip_prefix("https://").or_else(|| external_url.strip_prefix("http://")).unwrap_or(external_url);
    let host = no_scheme.split('/').next().unwrap_or("").split(':').next().unwrap_or("");
    if host.is_empty() { "codehaven.invalid" } else { host }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_address() {
        assert_eq!(noreply_email(7, "alice", "https://code.example.com"), "7+alice@users.noreply.code.example.com");
    }

    #[test]
    fn port_is_dropped() {
        assert_eq!(noreply_email(7, "alice", "http://code.localhost:3200"), "7+alice@users.noreply.code.localhost");
    }

    #[test]
    fn empty_origin_is_invalid() {
        assert_eq!(noreply_email(7, "alice", ""), "7+alice@users.noreply.codehaven.invalid");
    }

    #[test]
    fn parses_its_own_addresses() {
        assert_eq!(parse_noreply("7+alice@users.noreply.code.example.com", "https://code.example.com"), Some((7, "alice".into())));
    }

    #[test]
    fn comparison_ignores_case() {
        assert_eq!(parse_noreply("7+Alice@USERS.NOREPLY.CODE.EXAMPLE.COM", "https://code.example.com"), Some((7, "alice".into())));
    }

    #[test]
    fn other_hosts_do_not_match() {
        assert_eq!(parse_noreply("7+alice@users.noreply.other.example", "https://code.example.com"), None);
    }

    #[test]
    fn malformed_local_parts_are_rejected() {
        let url = "https://code.example.com";
        assert_eq!(parse_noreply("x+alice@users.noreply.code.example.com", url), None);
        assert_eq!(parse_noreply("7@users.noreply.code.example.com", url), None);
        assert_eq!(parse_noreply("7+@users.noreply.code.example.com", url), None);
        assert_eq!(parse_noreply("alice@example.com", url), None);
    }

    #[test]
    fn ids_that_do_not_fit_are_rejected() {
        assert_eq!(parse_noreply("99999999999999999999+alice@users.noreply.code.example.com", "https://code.example.com"), None);
    }
}
