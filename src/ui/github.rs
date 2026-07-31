use globset::Glob;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

use crate::boards::Board;

/// Build a ureq Agent with native-tls wired up. ureq's free functions
/// (`ureq::get(...)`) use a default Agent that doesn't know about native-tls
/// even when the feature is enabled, so we have to build one explicitly.
pub(crate) fn build_https_agent() -> ureq::Agent {
    let tls = native_tls::TlsConnector::new()
        .expect("native-tls connector should initialize on this platform");
    ureq::AgentBuilder::new()
        .tls_connector(Arc::new(tls))
        // Versioned UA per GitHub's recommendation.
        .user_agent(concat!("newerglow/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Duration::from_secs(15))
        // ureq's current default, pinned so a version bump can't change it.
        .redirects(5)
        .build()
}

/// Reject anything that isn't a plain `https://` URL before it reaches
/// the agent.
pub(crate) fn require_https(url: &str) -> Result<(), FetchError> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(FetchError::Network(format!("refusing non-https URL: {}", url)))
    }
}

/// A simplified release entry. We only keep the fields we render or download.
#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct Release {
    pub tag_name: String,
    pub name: Option<String>,
    pub html_url: String,
    pub published_at: Option<String>,
    #[serde(default)]
    pub draft: bool,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
    /// GitHub's per-asset content digest, e.g. `"sha256:abc123…"`. Present
    /// on releases published after GitHub added the field; older releases
    /// omit it, so it's optional and verification is best-effort.
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("network: {0}")]
    Network(String),
    #[error("rate limited (try again later)")]
    RateLimited,
    #[error("repo {owner}/{repo} not found")]
    NotFound { owner: String, repo: String },
    #[error("invalid response: {0}")]
    Parse(String),
}

/// Fetch releases for the given board. Filters to releases that have at
/// least one asset matching the board's `asset_pattern`. Sorted newest-first.
pub fn fetch_releases(board: &Board) -> Result<Vec<Release>, FetchError> {
    let fw = &board.manifest.firmware;

    let url = format!(
        "https://api.github.com/repos/{}/{}/releases",
        fw.github_owner, fw.github_repo
    );
    require_https(&url)?;

    let agent = build_https_agent();
    let resp = agent
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .call();

    let body = resp.map_err(|e| match e {
        ureq::Error::Status(404, _) => FetchError::NotFound {
            owner: fw.github_owner.clone(),
            repo: fw.github_repo.clone(),
        },
        ureq::Error::Status(403, r) | ureq::Error::Status(429, r) => {
            // 403 on GH is the rate-limit signal
            let msg = r.into_string().unwrap_or_default();
            if msg.to_ascii_lowercase().contains("rate limit") {
                FetchError::RateLimited
            } else {
                FetchError::Network(msg)
            }
        }
        ureq::Error::Status(code, r) => FetchError::Network(format!(
            "HTTP {}: {}",
            code,
            r.into_string().unwrap_or_default()
        )),
        ureq::Error::Transport(t) => FetchError::Network(t.to_string()),
    })?;

    let mut releases: Vec<Release> = body
        .into_json()
        .map_err(|e| FetchError::Parse(e.to_string()))?;

    let glob = Glob::new(&fw.asset_pattern)
        .map_err(|e| FetchError::Parse(format!("bad asset_pattern: {}", e)))?
        .compile_matcher();

    // Filter to non-draft releases that (a) have an asset matching the
    // board's pattern AND (b) carry a version-parseable tag. The version
    // requirement guarantees every listed release is comparable, which
    // we'll later need for hardware-minimum-version checks.
    releases.retain(|r| {
        !r.draft
            && r.assets.iter().any(|a| glob.is_match(&a.name))
            && parse_release_version(&r.tag_name, &fw.tag_prefix).is_some()
    });

    // Sort newest version first via loose numeric comparison. Stable
    // releases outrank pre-releases of the same version.
    releases.sort_by(|a, b| {
        let av = parse_release_version(&a.tag_name, &fw.tag_prefix).expect("filtered above");
        let bv = parse_release_version(&b.tag_name, &fw.tag_prefix).expect("filtered above");
        bv.cmp(&av)
    });

    Ok(releases)
}

/// Parse a tag of the form `<prefix><digits>(.<digits>)*<optional pre-release>`
/// into `(numeric_components, is_stable)`. Tags that don't carry `prefix` or
/// have no parseable digits return `None`. One `v` right after the prefix is
/// consumed, so an empty prefix accepts both `v1.2.3` and `1.2.3`.
///
/// Sort key semantics: `Vec<u64>` compares lexicographically (newer
/// version > older), and the `bool` puts stable releases above
/// pre-releases of the same numeric version (`true > false`).
///
/// Examples, with `prefix = "fw-v"`:
/// - `"fw-v1.2.3"` → `Some(([1, 2, 3], true))`
/// - `"fw-v1.2"` → `Some(([1, 2], true))`
/// - `"fw-v1.2.3-rc1"` → `Some(([1, 2, 3], false))`
/// - `"fw-vlatest"` → `None`
/// - `"v1.2.3"` → `None` (missing `fw-` prefix)
pub fn parse_release_version(tag: &str, prefix: &str) -> Option<(Vec<u64>, bool)> {
    let stripped = tag.strip_prefix(prefix)?;
    let stripped = stripped.strip_prefix('v').unwrap_or(stripped);
    let mut digits_end = 0;
    for (i, c) in stripped.char_indices() {
        if c.is_ascii_digit() || c == '.' {
            digits_end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if digits_end == 0 {
        return None;
    }
    let (digits, rest) = stripped.split_at(digits_end);
    let parts: Vec<u64> = digits
        .split('.')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u64>().unwrap_or(0))
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some((parts, rest.is_empty()))
}

impl Release {
    /// User-facing label for the dropdown. Falls back to tag_name if the
    /// release doesn't have a name.
    pub fn display_label(&self) -> String {
        let primary = self.name.as_deref().unwrap_or(&self.tag_name);
        match self.published_at.as_deref() {
            Some(ts) if ts.len() >= 10 => format!("{}  ({})", primary, &ts[..10]),
            _ => primary.to_string(),
        }
    }

    /// Pick the first asset matching the board's pattern.
    pub fn matching_asset(&self, pattern: &str) -> Option<&Asset> {
        let glob = Glob::new(pattern).ok()?.compile_matcher();
        self.assets.iter().find(|a| glob.is_match(&a.name))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_release_version, require_https, FetchError};

    #[test]
    fn require_https_accepts_https() {
        assert!(require_https("https://github.com/x/y").is_ok());
        assert!(require_https("https://api.github.com/repos/x/y/releases").is_ok());
        assert!(require_https("https://objects.githubusercontent.com/blob").is_ok());
    }

    #[test]
    fn require_https_rejects_non_https() {
        for url in [
            "http://github.com/x/y",
            "ftp://github.com/x/y",
            "file:///etc/passwd",
            "//github.com/x/y",
            "github.com/x/y",
            "",
        ] {
            match require_https(url) {
                Err(FetchError::Network(_)) => {}
                other => panic!("expected Network err for {:?}, got {:?}", url, other),
            }
        }
    }


    #[test]
    fn parses_simple_versions() {
        assert_eq!(parse_release_version("fw-v1.2.3", "fw-v"), Some((vec![1, 2, 3], true)));
        assert_eq!(parse_release_version("fw-v1.2", "fw-v"), Some((vec![1, 2], true)));
        assert_eq!(parse_release_version("fw-v2", "fw-v"), Some((vec![2], true)));
        assert_eq!(parse_release_version("fw-v2026.05.01", "fw-v"), Some((vec![2026, 5, 1], true)));
    }

    #[test]
    fn parses_prereleases() {
        assert_eq!(parse_release_version("fw-v1.2.3-rc1", "fw-v"), Some((vec![1, 2, 3], false)));
        assert_eq!(parse_release_version("fw-v1.0-alpha.1", "fw-v"), Some((vec![1, 0], false)));
        assert_eq!(parse_release_version("fw-v2+build.5", "fw-v"), Some((vec![2], false)));
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(parse_release_version("v1.2.3", "fw-v"), None); // no fw- prefix
        assert_eq!(parse_release_version("fw-vlatest", "fw-v"), None);
        assert_eq!(parse_release_version("fw-v", "fw-v"), None);
        assert_eq!(parse_release_version("random", "fw-v"), None);
        assert_eq!(parse_release_version("", "fw-v"), None);
    }

    #[test]
    fn sort_order_descending() {
        let mut tags = vec![
            "fw-v1.2.3",
            "fw-v1.10.0",
            "fw-v1.2.3-rc1",
            "fw-v2.0.0",
            "fw-v1.2",
        ];
        tags.sort_by(|a, b| {
            let av = parse_release_version(a, "fw-v").unwrap();
            let bv = parse_release_version(b, "fw-v").unwrap();
            bv.cmp(&av)
        });
        assert_eq!(
            tags,
            vec![
                "fw-v2.0.0",      // highest major
                "fw-v1.10.0",     // 10 > 2 numerically
                "fw-v1.2.3",      // stable beats rc1 of same version
                "fw-v1.2.3-rc1",
                "fw-v1.2",        // shorter loses on ties (1.2 < 1.2.3)
            ]
        );
    }
}
