//! The built-in actions a `uses:` may name.
//!
//! Codehaven runs a small set of actions itself, inside the runner, rather than fetching code
//! from anywhere. `actions/checkout@v4`, `codehaven/checkout@v1` and plain `checkout` are the
//! same action. Anything else is refused with a plain reason.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Builtin {
    Checkout,
    Cache,
    UploadArtifact,
    DownloadArtifact,
    /// `docker://image:tag`, run as one container with `args` and `entrypoint`.
    Docker {
        image: String,
    },
}

impl Builtin {
    pub fn name(&self) -> &'static str {
        match self {
            Builtin::Checkout => "checkout",
            Builtin::Cache => "cache",
            Builtin::UploadArtifact => "upload-artifact",
            Builtin::DownloadArtifact => "download-artifact",
            Builtin::Docker { .. } => "docker",
        }
    }

    /// `with:` keys that must be present.
    pub fn required_inputs(&self) -> &'static [&'static str] {
        match self {
            Builtin::Cache => &["key", "path"],
            Builtin::UploadArtifact => &["path"],
            _ => &[],
        }
    }

    /// `with:` keys the action reads. Others are ignored, because real workflows pass options
    /// such as `token` or `persist-credentials` that mean nothing here.
    pub fn known_inputs(&self) -> &'static [&'static str] {
        match self {
            Builtin::Checkout => &["ref", "fetch-depth", "path", "submodules"],
            Builtin::Cache => &["key", "restore-keys", "path"],
            Builtin::UploadArtifact => &["name", "path", "retention-days", "if-no-files-found"],
            Builtin::DownloadArtifact => &["name", "path"],
            Builtin::Docker { .. } => &["args", "entrypoint"],
        }
    }
}

/// Reads a `uses:` value. The error says why, in a sentence that ends the validation message.
pub fn classify(uses: &str) -> Result<Builtin, String> {
    let u = uses.trim();
    if u.is_empty() {
        return Err("`uses` is empty".into());
    }
    if let Some(image) = u.strip_prefix("docker://") {
        if image.is_empty() || image.chars().any(char::is_whitespace) {
            return Err("`docker://` needs an image name, such as docker://alpine:3.20".into());
        }
        return Ok(Builtin::Docker { image: image.to_string() });
    }
    if u.starts_with("./") || u.starts_with("../") {
        return Err(format!("`uses: {u}` is a composite action from the same repository, which is not supported yet"));
    }
    let (path, _version) = match u.split_once('@') {
        Some((p, v)) => (p, Some(v)),
        None => (u, None),
    };
    let name = match path.split('/').collect::<Vec<_>>().as_slice() {
        [n] => *n,
        [owner, n] if matches!(owner.to_ascii_lowercase().as_str(), "actions" | "codehaven") => *n,
        _ => "",
    };
    match name.to_ascii_lowercase().as_str() {
        "checkout" => Ok(Builtin::Checkout),
        "cache" => Ok(Builtin::Cache),
        "upload-artifact" => Ok(Builtin::UploadArtifact),
        "download-artifact" => Ok(Builtin::DownloadArtifact),
        _ => Err(format!(
            "`uses: {u}` is not supported. Codehaven runs checkout, cache, upload-artifact, download-artifact and docker:// images. \
             JavaScript actions from a marketplace are not supported, so run the tool in a `run:` step instead"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_action_by_every_spelling() {
        for s in ["checkout", "actions/checkout@v4", "codehaven/checkout@v1", "Actions/Checkout@main", "actions/checkout"] {
            assert_eq!(classify(s).unwrap(), Builtin::Checkout, "{s}");
        }
        assert_eq!(classify("actions/upload-artifact@v4").unwrap(), Builtin::UploadArtifact);
        assert_eq!(classify("actions/download-artifact@v4").unwrap(), Builtin::DownloadArtifact);
        assert_eq!(classify("actions/cache@v4").unwrap(), Builtin::Cache);
        assert_eq!(classify("docker://alpine:3.20").unwrap(), Builtin::Docker { image: "alpine:3.20".into() });
    }

    #[test]
    fn refuses_the_rest_with_a_reason() {
        let e = classify("actions/setup-node@v4").unwrap_err();
        assert!(e.contains("not supported") && e.contains("run:"), "{e}");
        assert!(classify("someone/checkout@v1").is_err(), "only actions/ and codehaven/ owners are built in");
        assert!(classify("actions/checkout/sub@v1").is_err());
        assert!(classify("./local").unwrap_err().contains("composite"));
        assert!(classify("docker://").is_err());
        assert!(classify("").is_err());
    }
}
