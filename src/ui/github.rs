use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

use crate::boards::{Board, HW_PLACEHOLDER};

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

    // Board-level filter: an asset for any hardware revision keeps the
    // release; which revision a device may use is decided per device.
    let glob = asset_matcher(&fw.asset_pattern, None)
        .map_err(|e| FetchError::Parse(format!("bad asset_pattern: {}", e)))?;

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
}

/// Compile `pattern` for one hardware revision, or for any (non-empty)
/// revision when `hw` is None. Without a `{hw}` placeholder, `hw` changes
/// nothing.
pub fn asset_matcher(pattern: &str, hw: Option<&str>) -> Result<GlobMatcher, globset::Error> {
    let pattern = pattern.replace(HW_PLACEHOLDER, hw.unwrap_or("?*"));
    Ok(Glob::new(&pattern)?.compile_matcher())
}

/// A device-reported revision usable in a glob: the contract allows
/// `[a-z0-9]+`, and anything carrying glob syntax (`*`, `[`, `{`) must not
/// reach the pattern, since IDENTIFY output is untrusted.
pub fn usable_hw(hw: &str) -> bool {
    !hw.is_empty() && hw.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The revision an asset name was published for, i.e. what `{hw}` stands
/// for in it. None when the pattern has no placeholder or doesn't match.
fn captured_hw(pattern: &str, name: &str) -> Option<String> {
    let (pre, post) = pattern.split_once(HW_PLACEHOLDER)?;
    let pre = Glob::new(pre).ok()?.compile_matcher();
    let post = Glob::new(post).ok()?.compile_matcher();
    // Earliest start and latest end: the longest capture, so `hw{hw}.uf2`
    // against "x-hwmini2.uf2" reads "mini2", not "2".
    for start in (0..=name.len()).filter(|&i| name.is_char_boundary(i)) {
        if !pre.is_match(&name[..start]) {
            continue;
        }
        for end in (start + 1..=name.len()).rev().filter(|&i| name.is_char_boundary(i)) {
            let hw = &name[start..end];
            if usable_hw(hw) && post.is_match(&name[end..]) {
                return Some(hw.to_string());
            }
        }
    }
    None
}

/// One flashable choice for a device: a release and the asset in it.
#[derive(Clone, Debug)]
pub struct Offer<'a> {
    pub release_idx: usize,
    pub release: &'a Release,
    pub asset: &'a Asset,
    pub label: String,
}

/// What a device may flash from a board's releases, newest first.
///
/// With a `{hw}` pattern and a known revision, each release offers its asset
/// for that revision, and releases without one aren't offered at all. With
/// the revision unknown, every revision's asset is offered, labeled with the
/// revision it was built for, so the user can choose. Without a placeholder
/// each release offers its first matching asset.
pub fn offers<'a>(releases: &'a [Release], pattern: &str, hw: Option<&str>) -> Vec<Offer<'a>> {
    let per_revision = pattern.contains(HW_PLACEHOLDER) && hw.is_none();
    let Ok(glob) = asset_matcher(pattern, hw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (release_idx, release) in releases.iter().enumerate() {
        let mut matching = release.assets.iter().filter(|a| glob.is_match(&a.name));
        if per_revision {
            for asset in matching {
                let label = match captured_hw(pattern, &asset.name) {
                    Some(hw) => format!("{} · hw {}", release.display_label(), hw),
                    None => format!("{} · {}", release.display_label(), asset.name),
                };
                out.push(Offer { release_idx, release, asset, label });
            }
        } else if let Some(asset) = matching.next() {
            out.push(Offer { release_idx, release, asset, label: release.display_label() });
        }
    }
    out
}

/// The release `Auto` picks for a device: the newest offer, except when the
/// board publishes per revision and the device's revision is unknown, where
/// guessing could hand it another board's image.
pub fn auto_offer<'a>(releases: &'a [Release], pattern: &str, hw: Option<&str>) -> Option<Offer<'a>> {
    if pattern.contains(HW_PLACEHOLDER) && hw.is_none() {
        return None;
    }
    offers(releases, pattern, hw).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::{
        asset_matcher, auto_offer, captured_hw, offers, parse_release_version, require_https,
        Asset, FetchError, Release,
    };

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

    fn release(tag: &str, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.to_string(),
            name: None,
            html_url: String::new(),
            published_at: None,
            draft: false,
            assets: assets
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    browser_download_url: format!("https://example.invalid/{n}"),
                    size: 0,
                    digest: None,
                })
                .collect(),
        }
    }

    const HEXA: &str = "motionhexa-*-hw{hw}.uf2";

    fn hexa_releases() -> Vec<Release> {
        vec![
            release("fw-v1.4", &["motionhexa-1.4-hw7.uf2", "motionhexa-1.4-hw8.uf2"]),
            release(
                "fw-v1.3",
                &["motionhexa-1.3-hw5.uf2", "motionhexa-1.3-hw6.uf2", "motionhexa-1.3-hw7.uf2", "motionhexa-1.3-hw8.uf2"],
            ),
            release("fw-v1.2", &["firmware-v5.uf2"]),
        ]
    }

    #[test]
    fn known_hw_is_offered_only_releases_with_its_asset() {
        let rs = hexa_releases();
        let v5: Vec<_> = offers(&rs, HEXA, Some("5")).iter().map(|o| o.asset.name.clone()).collect();
        assert_eq!(v5, ["motionhexa-1.3-hw5.uf2"]);
        let v8: Vec<_> = offers(&rs, HEXA, Some("8")).iter().map(|o| o.release_idx).collect();
        assert_eq!(v8, [0, 1]);
        assert!(offers(&rs, HEXA, Some("9")).is_empty());
    }

    #[test]
    fn auto_is_newest_eligible_not_newest_overall() {
        let rs = hexa_releases();
        let auto = auto_offer(&rs, HEXA, Some("6")).unwrap();
        assert_eq!(auto.release_idx, 1);
        assert_eq!(auto.asset.name, "motionhexa-1.3-hw6.uf2");
    }

    #[test]
    fn unknown_hw_lists_every_revision_and_never_auto_picks() {
        let rs = hexa_releases();
        let labels: Vec<_> = offers(&rs, HEXA, None).into_iter().map(|o| o.label).collect();
        assert_eq!(
            labels,
            ["fw-v1.4 · hw 7", "fw-v1.4 · hw 8", "fw-v1.3 · hw 5", "fw-v1.3 · hw 6", "fw-v1.3 · hw 7", "fw-v1.3 · hw 8"]
        );
        assert!(auto_offer(&rs, HEXA, None).is_none());
    }

    #[test]
    fn pattern_without_placeholder_keeps_first_match_behavior() {
        let rs = vec![release("fw-v1.2", &["notes.txt", "firmware-v5.uf2", "other.uf2"])];
        let auto = auto_offer(&rs, "*.uf2", None).unwrap();
        assert_eq!(auto.asset.name, "firmware-v5.uf2");
        assert_eq!(offers(&rs, "*.uf2", Some("7")).len(), 1, "hw is ignored without {{hw}}");
    }

    #[test]
    fn placeholder_is_not_left_to_glob_alternation() {
        // Unsubstituted, globset would read `{hw}` as the literal "hw".
        let any = asset_matcher(HEXA, None).unwrap();
        assert!(any.is_match("motionhexa-1.3-hw7.uf2"));
        assert!(!any.is_match("motionhexa-1.3-hw.uf2"));
        assert!(!asset_matcher(HEXA, Some("7")).unwrap().is_match("motionhexa-1.3-hwhw.uf2"));
    }

    #[test]
    fn captures_the_whole_revision() {
        assert_eq!(captured_hw(HEXA, "motionhexa-1.3-hwmini2.uf2").as_deref(), Some("mini2"));
        assert_eq!(captured_hw(HEXA, "motionhexa-1.4-rc1-hw7.uf2").as_deref(), Some("7"));
        assert_eq!(captured_hw("*.uf2", "a.uf2"), None);
    }

    #[test]
    fn embedded_patterns_compile_for_any_and_one_revision() {
        for board in crate::boards::Registry::load().boards() {
            let pattern = &board.manifest.firmware.asset_pattern;
            assert!(asset_matcher(pattern, None).is_ok(), "{}: {pattern}", board.id);
            assert!(asset_matcher(pattern, Some("7")).is_ok(), "{}: {pattern}", board.id);
        }
    }
}
