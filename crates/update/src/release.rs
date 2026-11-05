use serde::Deserialize;

use crate::error::UpdateError;
use crate::version::Version;

/// One downloadable file of a release.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Asset {
    pub name: String,
    pub browser_download_url: String,
    /// `sha256:<hex>`, as GitHub's API reports it.
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Deserialize)]
struct Raw {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

/// A published GitHub release.
#[derive(Debug, Clone)]
pub struct Release {
    version: Version,
    assets: Vec<Asset>,
}

impl Release {
    /// Parses the body of the `releases/latest` API call.
    ///
    /// # Errors
    ///
    /// Fails when the JSON is malformed or the tag is not a version.
    pub fn from_json(json: &[u8]) -> Result<Self, UpdateError> {
        let raw: Raw =
            serde_json::from_slice(json).map_err(|e| UpdateError::BadRelease(e.to_string()))?;
        let version = Version::parse(&raw.tag_name)
            .ok_or_else(|| UpdateError::BadRelease(format!("tag {:?}", raw.tag_name)))?;
        Ok(Self {
            version,
            assets: raw.assets,
        })
    }

    /// The release's version.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    pub(crate) fn asset_ending(&self, suffix: &'static str) -> Result<&Asset, UpdateError> {
        self.assets
            .iter()
            .find(|asset| asset.name.ends_with(suffix))
            .ok_or_else(|| UpdateError::NoAsset {
                version: self.version.to_string(),
                suffix,
            })
    }

    pub(crate) fn asset_named(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// The SHA-256 (lowercase hex) GitHub published for `asset`, if the API gave one.
pub(crate) fn digest_of(asset: &Asset) -> Option<String> {
    let hex = asset.digest.as_deref()?.strip_prefix("sha256:")?;
    Some(hex.to_ascii_lowercase())
}

/// Finds `name` in a `SHA256SUMS` file (`<hex>  <name>` lines).
pub(crate) fn sum_in(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hex.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{"tag_name":"v0.6.0","assets":[
        {"name":"tt500-lcd-0.6.0-linux-x86_64.tar.gz","browser_download_url":"http://h/a.tar.gz","digest":"sha256:ABCD"},
        {"name":"TT500-LCD-0.6.0-x86_64.AppImage","browser_download_url":"http://h/b.AppImage"},
        {"name":"SHA256SUMS","browser_download_url":"http://h/sums"}]}"#;

    #[test]
    fn picks_assets_by_suffix() {
        let release = Release::from_json(JSON.as_bytes()).unwrap();
        assert_eq!(release.version().to_string(), "0.6.0");
        let tar = release.asset_ending("-linux-x86_64.tar.gz").unwrap();
        assert_eq!(tar.browser_download_url, "http://h/a.tar.gz");
        assert!(release.asset_ending("-x86_64.AppImage").is_ok());
        assert!(release.asset_ending(".zip").is_err());
        assert!(release.asset_named("SHA256SUMS").is_some());
    }

    #[test]
    fn digest_is_parsed_and_lowercased() {
        let release = Release::from_json(JSON.as_bytes()).unwrap();
        let tar = release.asset_ending(".tar.gz").unwrap();
        assert_eq!(digest_of(tar).as_deref(), Some("abcd"));
        let image = release.asset_ending(".AppImage").unwrap();
        assert_eq!(digest_of(image), None);
    }

    #[test]
    fn finds_a_line_in_sha256sums() {
        let sums = "AA11  one.tar.gz\nbb22 *two.AppImage\n";
        assert_eq!(sum_in(sums, "two.AppImage").as_deref(), Some("bb22"));
        assert_eq!(sum_in(sums, "one.tar.gz").as_deref(), Some("aa11"));
        assert_eq!(sum_in(sums, "three"), None);
    }

    #[test]
    fn rejects_bad_json_and_tags() {
        assert!(Release::from_json(b"{").is_err());
        assert!(Release::from_json(br#"{"tag_name":"nightly"}"#).is_err());
    }
}
