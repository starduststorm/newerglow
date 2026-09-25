//! IDENTIFY response parser.
//!
//! dustlib's `NewerGlowUpdater` emits responses of the form (see
//! `doc/firmware-release-contract.md`):
//!
//! ```text
//! ID:<product> v<fw_version>[ hw=<hw_version>][ sn=<board_id>]
//! ```
//!
//! The `ID:` prefix is stripped by `identify::try_identify` before the
//! string reaches the UI; what we receive here is the post-prefix portion.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedIdentity {
    pub product: String,
    pub fw_version: Option<String>,
    pub hw_version: Option<String>,
    pub serial_number: Option<String>,
}

impl ParsedIdentity {
    /// Parse the post-`ID:` body. Tokens are whitespace-separated.
    /// The first token is the product name; subsequent tokens are
    /// recognised by prefix:
    /// - `v<…>`   → firmware version (the leading 'v' is stripped)
    /// - `hw=<…>` → hardware version
    /// - `sn=<…>` → serial number
    ///
    /// Unknown tokens are silently ignored.
    pub fn parse(s: &str) -> Self {
        let mut tokens = s.split_whitespace();
        let product = tokens.next().unwrap_or("").to_string();
        let mut id = ParsedIdentity {
            product,
            ..Default::default()
        };
        for tok in tokens {
            if let Some(v) = tok.strip_prefix('v') {
                if id.fw_version.is_none() && !v.is_empty() {
                    id.fw_version = Some(v.to_string());
                }
            } else if let Some(v) = tok.strip_prefix("hw=") {
                id.hw_version = Some(v.to_string());
            } else if let Some(v) = tok.strip_prefix("sn=") {
                id.serial_number = Some(v.to_string());
            }
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::ParsedIdentity;

    #[test]
    fn parses_full_identity() {
        let id = ParsedIdentity::parse("motionhexa v0.9.0 hw=5 sn=DEADBEEF");
        assert_eq!(id.product, "motionhexa");
        assert_eq!(id.fw_version.as_deref(), Some("0.9.0"));
        assert_eq!(id.hw_version.as_deref(), Some("5"));
        assert_eq!(id.serial_number.as_deref(), Some("DEADBEEF"));
    }

    #[test]
    fn parses_untagged_dev_build() {
        // Verbatim from a penta built outside a release tag.
        let id = ParsedIdentity::parse("penta v0.0.0+g0776467.dirty hw=2 sn=374796340604105F");
        assert_eq!(id.product, "penta");
        assert_eq!(id.fw_version.as_deref(), Some("0.0.0+g0776467.dirty"));
        assert_eq!(id.hw_version.as_deref(), Some("2"));
        assert_eq!(id.serial_number.as_deref(), Some("374796340604105F"));
    }

    #[test]
    fn parses_missing_optional_fields() {
        let id = ParsedIdentity::parse("widget v1.2");
        assert_eq!(id.product, "widget");
        assert_eq!(id.fw_version.as_deref(), Some("1.2"));
        assert_eq!(id.hw_version, None);
        assert_eq!(id.serial_number, None);
    }

    #[test]
    fn parses_missing_fw_version() {
        let id = ParsedIdentity::parse("widget hw=v3 sn=ABC");
        assert_eq!(id.product, "widget");
        assert_eq!(id.fw_version, None);
        assert_eq!(id.hw_version.as_deref(), Some("v3"));
        assert_eq!(id.serial_number.as_deref(), Some("ABC"));
    }

    #[test]
    fn empty_string_yields_empty_identity() {
        let id = ParsedIdentity::parse("");
        assert_eq!(id.product, "");
        assert_eq!(id.fw_version, None);
    }

    #[test]
    fn ignores_unknown_tokens() {
        let id = ParsedIdentity::parse("widget v1.0 region=us extra-stuff");
        assert_eq!(id.fw_version.as_deref(), Some("1.0"));
        assert_eq!(id.hw_version, None);
        assert_eq!(id.serial_number, None);
    }

    #[test]
    fn does_not_misread_lone_v() {
        // Bare "v" (no version after it) shouldn't set fw_version
        let id = ParsedIdentity::parse("widget v");
        assert_eq!(id.fw_version, None);
    }

    #[test]
    fn first_v_token_wins() {
        // Defensive: if firmware accidentally emits two v-tokens
        let id = ParsedIdentity::parse("widget v1.0 v2.0");
        assert_eq!(id.fw_version.as_deref(), Some("1.0"));
    }
}
