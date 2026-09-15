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

    #[test]
    fn key_kind_renders_as_a_metric_label() {
        assert_eq!(KeyKind::Index.to_string(), "index");
        assert_eq!(KeyKind::Releases.to_string(), "releases");
        assert_eq!(KeyKind::Latest.to_string(), "latest");
        assert_eq!(KeyKind::Config.to_string(), "config");
    }
}
