use serde::Serialize;

use crate::keys::Coordinate;
use crate::model::{AssetKind, Index, Release};
use crate::resolve;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DeriveError {
    #[error("release {version} has no {kind} asset")]
    NoAsset { version: String, kind: &'static str },
}

/// The Squirrel.Mac manifest. Field declaration order IS the serialized
/// order, and the golden fixture asserts on it, so do not reorder.
#[derive(Debug, Serialize)]
pub struct LatestManifest {
    pub url: String,
    pub name: String,
    pub notes: String,
    pub pub_date: String,
}

/// Derive `latest.json` for a coordinate, or `Ok(None)` when no release is
/// available to every client.
///
/// `rollout_pct` is deliberately not honoured per-client here: `latest.json`
/// is one stored object shared by the whole fleet, so it can only describe a
/// release at 100%. Per-client rollout filtering happens in the server
/// handler against `index.json`.
///
/// `url` points at tanukistore's own download route rather than a presigned
/// MinIO URL (spec 4.7), which is what keeps these bytes static and lets the
/// server return them without re-serializing.
pub fn derive_latest(
    index: &Index,
    coord: &Coordinate,
    base_url: &str,
) -> Result<Option<Vec<u8>>, DeriveError> {
    let Some(release) = resolve::resolve_eligible(index, &coord.app, &coord.channel, None) else {
        return Ok(None);
    };
    let asset = pick(release, AssetKind::Zip, "zip")?;
    let base = base_url.trim_end_matches('/');
    let manifest = LatestManifest {
        url: format!(
            "{base}/download/{app}/{version}?platform={platform}&arch={arch}&channel={channel}&filename={filename}",
            app = coord.app,
            version = release.version,
            platform = coord.platform,
            arch = coord.arch,
            channel = coord.channel,
            filename = asset.filename,
        ),
        name: release.version.to_string(),
        notes: release.notes.clone(),
        pub_date: release
            .pub_date
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    };
    Ok(Some(serde_json::to_vec(&manifest).expect(
        "LatestManifest is plain owned data and cannot fail to serialize",
    )))
}

fn pick<'a>(
    release: &'a Release,
    kind: AssetKind,
    label: &'static str,
) -> Result<&'a crate::model::Asset, DeriveError> {
    release
        .assets
        .iter()
        .find(|asset| asset.kind == kind)
        .ok_or_else(|| DeriveError::NoAsset {
            version: release.version.to_string(),
            kind: label,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arch, Asset, AssetKind, Index, Platform, Release, RolloutPct};
    use chrono::{TimeZone, Utc};
    use semver::Version;

    fn coord() -> Coordinate {
        Coordinate::new("myapp", "stable", Platform::Darwin, Arch::Arm64)
    }

    fn zip_release(version: &str, pct: u8) -> Release {
        Release {
            version: Version::parse(version).unwrap(),
            notes: "Fixes a crash on launch.".to_owned(),
            pub_date: Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap(),
            rollout_pct: RolloutPct::new(pct).unwrap(),
            assets: vec![Asset {
                kind: AssetKind::Zip,
                filename: format!("myapp-{version}-darwin-arm64.zip"),
                sha1: "da39a3ee5e6b4b0d3255bfef95601890afd80709".to_owned(),
                sha512: "cf83e1357eefb8bd".to_owned(),
                size_bytes: 92_341_120,
            }],
        }
    }

    #[test]
    fn matches_the_golden_fixture_byte_for_byte() {
        let index = Index { releases: vec![zip_release("1.5.0", 100)] };
        let bytes = derive_latest(&index, &coord(), "https://updates.example.com")
            .unwrap()
            .expect("a release is eligible");
        let expected = include_str!("../tests/fixtures/latest-darwin-arm64.json");
        assert_eq!(String::from_utf8(bytes).unwrap(), expected.trim_end());
    }

    #[test]
    fn describes_only_the_newest_release() {
        let index = Index {
            releases: vec![zip_release("1.4.0", 100), zip_release("1.5.0", 100)],
        };
        let bytes = derive_latest(&index, &coord(), "https://updates.example.com")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("1.5.0"), "{text}");
        assert!(!text.contains("1.4.0"), "{text}");
    }

    #[test]
    fn a_trailing_slash_on_base_url_does_not_double_up() {
        let index = Index { releases: vec![zip_release("1.5.0", 100)] };
        let bytes = derive_latest(&index, &coord(), "https://updates.example.com/")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("com//"), "{text}");
    }

    #[test]
    fn a_partial_rollout_is_not_published_to_latest_json() {
        let index = Index { releases: vec![zip_release("1.5.0", 10)] };
        assert!(
            derive_latest(&index, &coord(), "https://updates.example.com")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn staged_release_falls_back_to_the_newest_full_release() {
        // 1.5.0 is staged at 10%, so latest.json must still describe 1.4.0 —
        // the newest release every client may have. Eligible clients are
        // upgraded by the server from index.json, not from latest.json.
        let index = Index {
            releases: vec![zip_release("1.4.0", 100), zip_release("1.5.0", 10)],
        };
        let bytes = derive_latest(&index, &coord(), "https://updates.example.com")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("1.4.0"), "{text}");
        assert!(!text.contains("1.5.0"), "{text}");
    }

    #[test]
    fn empty_index_derives_nothing() {
        let index = Index::default();
        assert!(
            derive_latest(&index, &coord(), "https://updates.example.com")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_release_with_no_zip_is_an_error() {
        let mut release = zip_release("1.5.0", 100);
        release.assets[0].kind = AssetKind::Dmg;
        let index = Index { releases: vec![release] };
        assert!(derive_latest(&index, &coord(), "https://updates.example.com").is_err());
    }
}
