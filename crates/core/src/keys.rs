use std::fmt;

use semver::Version;

use crate::model::{Arch, Platform};

/// Why an operator-supplied `app` or `channel` cannot be used in a key.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CoordinateError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error(
        "{field} {value:?} contains {ch:?}; only ASCII letters, digits, '.', '_' and '-' are allowed"
    )]
    IllegalCharacter {
        field: &'static str,
        value: String,
        ch: char,
    },
    #[error("{field} {value:?} is a relative path component")]
    RelativePathComponent { field: &'static str, value: String },
}

/// Check that a value is usable as an `app` or `channel` everywhere the
/// protocol interpolates one.
///
/// These two strings are operator-supplied and reach four sinks, each with a
/// different separator that must not appear in them:
///
/// - the object key `{app}/{channel}/{platform}/{arch}/...` (spec 5). Without
///   this check `(app="a/b", channel="c")` and `(app="a", channel="b/c")`
///   produce the SAME prefix, so two apps silently share - and overwrite -
///   each other's index, manifests and assets.
/// - the `/download/:app/:version` route, where `app` is one path segment, so
///   a `/`, `?` or `#` changes the route's shape rather than its contents.
/// - the rollout bucket hash over `{app}:{channel}:{id}`
///   ([`crate::rollout::bucket`], a frozen wire contract), where a `:` makes
///   `("a:b", "c")` and `("a", "b:c")` hash identically and therefore share a
///   rollout cohort.
/// - the NATS subject `updates.versioncheck.{app}.{uid}.{channel}.{platform}`
///   (spec 9), where `.`, ` `, `*` and `>` are all structural.
///
/// The allowlist is `[A-Za-z0-9._-]`, chosen over the stricter `[A-Za-z0-9_-]`
/// so that a reverse-DNS appId such as `com.example.app` - Electron's own
/// convention, and what most operators will already have - remains usable.
///
/// UNRESOLVED, and the price of admitting `.`: an app containing dots expands
/// into several NATS subject tokens, so the per-user replay filter spec 9
/// documents as `updates.versioncheck.*.{uid}.>` no longer matches it. Spec 9
/// needs to either encode the app token or widen that filter. Nothing in this
/// crate publishes NATS subjects yet, so the gap is not live, but it must be
/// closed before the observability path ships.
///
/// `.` and `..` are rejected outright: they are legal under the allowlist but
/// are relative path components, and an `app` of `..` escapes the key prefix
/// wherever a key is used as a path - which the local disk cache tier does.
pub fn validate_segment(field: &'static str, value: &str) -> Result<(), CoordinateError> {
    if value.is_empty() {
        return Err(CoordinateError::Empty { field });
    }
    if value == "." || value == ".." {
        return Err(CoordinateError::RelativePathComponent {
            field,
            value: value.to_owned(),
        });
    }
    match value
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')))
    {
        Some(ch) => Err(CoordinateError::IllegalCharacter {
            field,
            value: value.to_owned(),
            ch,
        }),
        None => Ok(()),
    }
}

/// One app/channel/platform/arch combination - the unit that owns an
/// `index.json` and its two derived manifests.
///
/// Fields are private so that [`Coordinate::new`] is the only way to build
/// one. A `pub` field would let a caller assemble an unvalidated coordinate
/// with a struct literal and walk straight past [`validate_segment`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Coordinate {
    app: String,
    channel: String,
    platform: Platform,
    arch: Arch,
}

impl Coordinate {
    /// Build a coordinate, rejecting an `app` or `channel` that cannot be
    /// interpolated unambiguously. See [`validate_segment`] for what "cannot"
    /// means and why this is fallible at all.
    pub fn new(
        app: impl Into<String>,
        channel: impl Into<String>,
        platform: Platform,
        arch: Arch,
    ) -> Result<Self, CoordinateError> {
        let app = app.into();
        let channel = channel.into();
        validate_segment("app", &app)?;
        validate_segment("channel", &channel)?;
        Ok(Coordinate {
            app,
            channel,
            platform,
            arch,
        })
    }

    pub fn app(&self) -> &str {
        &self.app
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    pub fn arch(&self) -> Arch {
        self.arch
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
    /// `app` and `version` are interpolated into the path unencoded, and both
    /// are safe there by construction rather than by convention: `version` is a
    /// `semver::Version`, whose grammar admits only alphanumerics, `-`, `.` and
    /// `+` (all legal in a path segment, and `+` is not space-decoded there),
    /// and `app` passed [`validate_segment`] at construction, whose allowlist
    /// `[A-Za-z0-9._-]` is a subset of the unreserved path characters.
    pub fn download_url(&self, base_url: &str, version: &Version, filename: &str) -> String {
        let base = base_url.trim_end_matches('/');
        let app = &self.app;
        // Pair order is the emitted query order and the golden fixture asserts
        // on it, so do not reorder. Frozen, not verified — see
        // tests/fixtures/README.md.
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("platform", &self.platform.to_string())
            .append_pair("arch", &self.arch.to_string())
            .append_pair("channel", &self.channel)
            .append_pair("filename", filename)
            .finish();
        format!("{base}/download/{app}/{version}?{query}")
    }
}

/// The per-app config object (spec 5). Fallible for the same reason
/// [`Coordinate::new`] is: `app` is operator-supplied and becomes a key prefix.
pub fn config_key(app: &str) -> Result<String, CoordinateError> {
    validate_segment("app", app)?;
    Ok(format!("{app}/config.json"))
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
        Coordinate::new("myapp", "stable", Platform::Darwin, Arch::Arm64).unwrap()
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
        assert_eq!(config_key("myapp").unwrap(), "myapp/config.json");
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
        )
        .unwrap();
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
    fn a_separator_in_app_or_channel_is_rejected() {
        // The collision this exists to prevent: with an unvalidated `/`, these
        // two DIFFERENT coordinates produce the SAME object prefix, so each
        // app reads and overwrites the other's index, manifests and assets.
        assert_eq!(
            format!("{}/{}/darwin/arm64", "a/b", "c"),
            format!("{}/{}/darwin/arm64", "a", "b/c"),
            "the ambiguity is real, which is why both inputs must be refused"
        );
        for (app, channel, field, value) in [
            ("a/b", "c", "app", "a/b"),
            ("a", "b/c", "channel", "b/c"),
        ] {
            assert_eq!(
                Coordinate::new(app, channel, Platform::Darwin, Arch::Arm64),
                Err(CoordinateError::IllegalCharacter {
                    field,
                    value: value.to_owned(),
                    ch: '/',
                })
            );
        }

        // One representative per sink, so a future widening of the allowlist
        // has to take each of these deliberately rather than by accident.
        let illegal = [
            ('/', "the object key and the /download/:app path segment"),
            (':', "the {app}:{channel}:{id} rollout bucket hash"),
            (' ', "the NATS subject, and NSURL(string:)"),
            ('*', "the NATS subject wildcard"),
            ('>', "the NATS subject wildcard"),
            ('?', "the /download query string"),
            ('#', "a URL fragment"),
            ('%', "percent-encoding"),
        ];
        for (ch, why) in illegal {
            let app = format!("my{ch}app");
            assert_eq!(
                Coordinate::new(&app, "stable", Platform::Darwin, Arch::Arm64),
                Err(CoordinateError::IllegalCharacter {
                    field: "app",
                    value: app.clone(),
                    ch,
                }),
                "{ch:?} must be rejected: it is structural in {why}"
            );
            assert!(
                config_key(&app).is_err(),
                "config_key takes the same operator-supplied app: {app:?}"
            );
        }
    }

    #[test]
    fn a_relative_path_component_is_rejected() {
        // Legal under the [A-Za-z0-9._-] allowlist, but `..` escapes the key
        // prefix anywhere a key is used as a filesystem path - which the local
        // disk cache tier does.
        for value in [".", ".."] {
            assert_eq!(
                Coordinate::new(value, "stable", Platform::Darwin, Arch::Arm64),
                Err(CoordinateError::RelativePathComponent {
                    field: "app",
                    value: value.to_owned(),
                })
            );
            assert_eq!(
                Coordinate::new("myapp", value, Platform::Darwin, Arch::Arm64),
                Err(CoordinateError::RelativePathComponent {
                    field: "channel",
                    value: value.to_owned(),
                })
            );
        }
    }

    #[test]
    fn an_empty_app_or_channel_is_rejected() {
        // "" would collapse the prefix to `/stable/darwin/arm64`, colliding
        // with every other empty-app coordinate.
        assert_eq!(
            Coordinate::new("", "stable", Platform::Darwin, Arch::Arm64),
            Err(CoordinateError::Empty { field: "app" })
        );
        assert_eq!(
            Coordinate::new("myapp", "", Platform::Darwin, Arch::Arm64),
            Err(CoordinateError::Empty { field: "channel" })
        );
        assert_eq!(config_key(""), Err(CoordinateError::Empty { field: "app" }));
    }

    #[test]
    fn the_names_operators_actually_use_are_accepted() {
        // `.` is in the allowlist specifically so Electron's reverse-DNS appId
        // convention keeps working. See `validate_segment`'s UNRESOLVED note on
        // what that costs the NATS subject hierarchy.
        for app in ["myapp", "my-app", "my_app_2", "com.example.app", "MyApp"] {
            assert!(
                Coordinate::new(app, "stable", Platform::Darwin, Arch::Arm64).is_ok(),
                "{app:?}"
            );
            assert!(config_key(app).is_ok(), "{app:?}");
        }
        for channel in ["stable", "beta", "next-rc", "1.x"] {
            assert!(
                Coordinate::new("myapp", channel, Platform::Darwin, Arch::Arm64).is_ok(),
                "{channel:?}"
            );
        }
    }

    #[test]
    fn key_kind_renders_as_a_metric_label() {
        assert_eq!(KeyKind::Index.to_string(), "index");
        assert_eq!(KeyKind::Releases.to_string(), "releases");
        assert_eq!(KeyKind::Latest.to_string(), "latest");
        assert_eq!(KeyKind::Config.to_string(), "config");
    }
}
