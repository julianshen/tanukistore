use std::fmt;

use semver::Version;

use crate::model::{Arch, Platform};

/// One app/channel/platform/arch combination — the unit that owns an
/// `index.json` and its two derived manifests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Coordinate {
    pub app: String,
    pub channel: String,
    pub platform: Platform,
    pub arch: Arch,
}

impl Coordinate {
    pub fn new(
        app: impl Into<String>,
        channel: impl Into<String>,
        platform: Platform,
        arch: Arch,
    ) -> Self {
        Coordinate {
            app: app.into(),
            channel: channel.into(),
            platform,
            arch,
        }
    }

    pub fn prefix(&self) -> String {
        format!("{}/{}/{}/{}", self.app, self.channel, self.platform, self.arch)
    }

    pub fn index_key(&self) -> String {
        format!("{}/index.json", self.prefix())
    }

    pub fn latest_key(&self) -> String {
        format!("{}/latest.json", self.prefix())
    }

    pub fn releases_key(&self) -> String {
        format!("{}/RELEASES", self.prefix())
    }

    pub fn asset_key(&self, version: &Version, filename: &str) -> String {
        format!("{}/{}/{}", self.prefix(), version, filename)
    }

    /// Absolute URL for tanukistore's own download route for one asset, as
    /// embedded in `latest.json` (spec 4.7).
    ///
    /// This is the inverse of [`Coordinate::asset_key`]: the path and query
    /// together carry every field the download handler needs to rebuild that
    /// object key, which is why both live in this one function rather than in
    /// a `format!` inside `derive`.
    ///
    /// Query values are serialized as `application/x-www-form-urlencoded`,
    /// which is exactly the grammar the handler will decode with (axum's
    /// `Query` extractor uses `serde_urlencoded`). Two concrete failures this
    /// prevents:
    ///
    /// - A raw space — electron-builder's default mac artifact name is
    ///   `${productName}-${version}-mac.zip`, and product names routinely
    ///   contain one — is not a legal URL character, and Squirrel.Mac's
    ///   `NSURL(string:)` returns nil for it, so the client silently never
    ///   updates.
    /// - A raw `+`, which semver build metadata puts into filenames, DECODES
    ///   TO A SPACE under form-urlencoding, so the handler would rebuild a
    ///   different object key and 404.
    ///
    /// `app` and `version` are interpolated into the path unencoded: `version`
    /// is a `semver::Version`, whose grammar admits only alphanumerics, `-`,
    /// `.` and `+` (all legal in a path segment, and `+` is not space-decoded
    /// there), and `app` is a bucket key segment (spec 5) constrained by the
    /// publisher.
    pub fn download_url(&self, base_url: &str, version: &Version, filename: &str) -> String {
        let base = base_url.trim_end_matches('/');
        let app = &self.app;
        // Pair order is the emitted query order and the golden fixture asserts
        // on it, so do not reorder.
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("platform", &self.platform.to_string())
            .append_pair("arch", &self.arch.to_string())
            .append_pair("channel", &self.channel)
            .append_pair("filename", filename)
            .finish();
        format!("{base}/download/{app}/{version}?{query}")
    }
}

pub fn config_key(app: &str) -> String {
    format!("{app}/config.json")
}

/// Bounded classification of cacheable objects, used as the `key_kind`
/// metric label. Deliberately low-cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Config,
    Index,
    Latest,
    Releases,
}

impl fmt::Display for KeyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            KeyKind::Config => "config",
            KeyKind::Index => "index",
            KeyKind::Latest => "latest",
            KeyKind::Releases => "releases",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arch, Platform};

    fn coord() -> Coordinate {
        Coordinate::new("myapp", "stable", Platform::Darwin, Arch::Arm64)
    }

    #[test]
    fn keys_match_the_documented_layout() {
        let c = coord();
        assert_eq!(c.prefix(), "myapp/stable/darwin/arm64");
        assert_eq!(c.index_key(), "myapp/stable/darwin/arm64/index.json");
        assert_eq!(c.latest_key(), "myapp/stable/darwin/arm64/latest.json");
        assert_eq!(c.releases_key(), "myapp/stable/darwin/arm64/RELEASES");
        assert_eq!(
            c.asset_key(&semver::Version::new(1, 5, 0), "app.zip"),
            "myapp/stable/darwin/arm64/1.5.0/app.zip"
        );
        assert_eq!(config_key("myapp"), "myapp/config.json");
    }

    /// Split an emitted download URL back into the pieces a download handler
    /// would extract: `{app}` and `{version}` from the path, the rest from the
    /// form-urlencoded query.
    fn parse_download_url(url: &str) -> (String, String, Vec<(String, String)>) {
        let (path, query) = url.split_once('?').expect("a query is always emitted");
        let rest = path
            .split_once("/download/")
            .expect("the download route is always present")
            .1;
        let (app, version) = rest.split_once('/').expect("path carries app and version");
        let pairs = form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        (app.to_owned(), version.to_owned(), pairs)
    }

    fn query_value(pairs: &[(String, String)], key: &str) -> String {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("missing {key}"))
    }

    #[test]
    fn download_url_round_trips_back_into_the_asset_key() {
        // Spec 4.7's whole point: the URL embedded in latest.json must resolve
        // to an object key that exists. Assert it by reconstructing the key
        // from nothing but the emitted URL.
        let c = coord();
        let version = semver::Version::parse("1.5.0").unwrap();
        let filename = "myapp-1.5.0-darwin-arm64.zip";
        let url = c.download_url("https://updates.example.com", &version, filename);

        let (app, version_str, pairs) = parse_download_url(&url);
        let rebuilt = Coordinate::new(
            app,
            query_value(&pairs, "channel"),
            query_value(&pairs, "platform").parse::<Platform>().unwrap(),
            query_value(&pairs, "arch").parse::<Arch>().unwrap(),
        );
        assert_eq!(rebuilt, c, "{url}");
        assert_eq!(
            rebuilt.asset_key(
                &semver::Version::parse(&version_str).unwrap(),
                &query_value(&pairs, "filename")
            ),
            c.asset_key(&version, filename),
            "{url}"
        );
    }

    #[test]
    fn download_url_query_values_round_trip_through_form_urlencoded_decoding() {
        // A space (electron-builder's `${productName}-${version}-mac.zip`), a
        // `+` (semver build metadata) and an `&` (the query separator) in one
        // filename. Each is silently destructive if emitted raw.
        let c = coord();
        let version = semver::Version::parse("1.5.0+build.7").unwrap();
        let filename = "My App-1.5.0+build.7&x-mac.zip";
        let url = c.download_url("https://updates.example.com", &version, filename);

        assert_eq!(
            url,
            "https://updates.example.com/download/myapp/1.5.0+build.7\
             ?platform=darwin&arch=arm64&channel=stable\
             &filename=My+App-1.5.0%2Bbuild.7%26x-mac.zip"
        );

        let (app, version_str, pairs) = parse_download_url(&url);
        assert_eq!(app, "myapp");
        assert_eq!(version_str, "1.5.0+build.7");
        assert_eq!(
            pairs,
            vec![
                ("platform".to_owned(), "darwin".to_owned()),
                ("arch".to_owned(), "arm64".to_owned()),
                ("channel".to_owned(), "stable".to_owned()),
                ("filename".to_owned(), filename.to_owned()),
            ],
            "every query value must decode byte-identically: {url}"
        );
        assert!(
            !url.contains(' '),
            "a raw space makes NSURL(string:) return nil: {url}"
        );
        // The asset key must survive the trip too, or the download 404s.
        assert_eq!(
            c.asset_key(
                &semver::Version::parse(&version_str).unwrap(),
                &query_value(&pairs, "filename")
            ),
            c.asset_key(&version, filename),
            "{url}"
        );
    }

    #[test]
    fn download_url_does_not_double_up_a_trailing_slash() {
        let c = coord();
        let version = semver::Version::new(1, 5, 0);
        assert_eq!(
            c.download_url("https://updates.example.com/", &version, "app.zip"),
            c.download_url("https://updates.example.com", &version, "app.zip")
        );
    }

    #[test]
    fn key_kind_renders_as_a_metric_label() {
        assert_eq!(KeyKind::Index.to_string(), "index");
        assert_eq!(KeyKind::Releases.to_string(), "releases");
        assert_eq!(KeyKind::Latest.to_string(), "latest");
        assert_eq!(KeyKind::Config.to_string(), "config");
    }
}
