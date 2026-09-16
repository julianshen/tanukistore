use serde::Serialize;
use url::{ParseError, Url};

use crate::keys::Coordinate;
use crate::model::{AssetKind, Index, Release};
use crate::resolve;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DeriveError {
    #[error("release {version} has no {kind} asset")]
    NoAsset { version: String, kind: AssetKind },
    #[error("two index entries share version {version}")]
    DuplicateVersion { version: String },
    #[error("filename {filename:?} contains whitespace, which the RELEASES line format cannot represent")]
    FilenameHasWhitespace { filename: String },
    #[error("sha1 {sha1:?} for {filename:?} is not 40 hex characters")]
    MalformedSha1 { filename: String, sha1: String },
    #[error("base_url {base_url:?} is unusable: {reason}")]
    UnusableBaseUrl {
        base_url: String,
        reason: &'static str,
    },
    #[error(
        "release {version} has rollout_pct {rollout_pct}, but Squirrel.Windows compares versions          itself, so a partial rollout cannot be staged on win32 - publish it at 100 or hold it back"
    )]
    StagedRolloutUnsupportedOnWindows { version: String, rollout_pct: u8 },
}

/// The Squirrel.Mac manifest. Field declaration order IS the serialized
/// order, and the golden fixture asserts on it, so do not reorder. Note that
/// this makes the order *frozen*, not *verified*: the fixture encodes our
/// belief about the wire format and no real Squirrel client has accepted it
/// yet. See `tests/fixtures/README.md` and spec 12 item 1.
///
/// Private: `derive_latest` returns serialized bytes, so no public function
/// hands one of these out, and it carries neither `Deserialize` nor
/// `PartialEq`, which is what a caller would need to do anything with it.
#[derive(Debug, Serialize)]
struct LatestManifest {
    url: String,
    name: String,
    notes: String,
    pub_date: String,
}

/// Check that `base_url` can actually be joined into an absolute download URL.
///
/// Spec 4.7 requires `latest.json`'s `url` to be ABSOLUTE, because Squirrel.Mac
/// hands it to `NSURLSession`.
///
/// This PARSES rather than pattern-matches, because every cheaper check has a
/// hole: `"https://"` satisfies a `starts_with("https://")` yet has no
/// authority, and `"https://:443"` satisfies "the first slash-delimited
/// substring is non-empty" yet still has no host.
///
/// The round-trip against [`Url::as_str`] is the load-bearing part, and the
/// reason parsing alone is not enough. [`crate::keys::Coordinate::download_url`]
/// formats with the RAW string, never with anything parsed, so validating only
/// the parse would let the two disagree: `"https:///download"` parses as host
/// `download` (the URL grammar collapses the extra slash), and
/// `"https://ex ample.com/"` parses only because the space is percent-encoded
/// on the way in - the same raw space that makes `NSURL(string:)` return nil,
/// silently stopping every macOS update. Demanding the input already be in
/// normalized form is what makes "the string we validated" and "the string we
/// emit" provably the same string.
///
/// Exposed so the publisher can reject a bad configuration at startup rather
/// than at the first derive.
pub fn validate_base_url(base_url: &str) -> Result<(), DeriveError> {
    let unusable = |reason: &'static str| DeriveError::UnusableBaseUrl {
        base_url: base_url.to_owned(),
        reason,
    };
    let parsed = Url::parse(base_url).map_err(|err| match err {
        ParseError::RelativeUrlWithoutBase => unusable("must start with http:// or https://"),
        ParseError::EmptyHost => unusable("has a scheme but no host"),
        ParseError::InvalidPort => unusable("has an invalid port"),
        _ => unusable("has an unparseable host"),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(unusable("must start with http:// or https://"));
    }
    // A query or fragment on the base is unjoinable - `download_url` appends
    // the path and its own query AFTER it, so both would land in the wrong
    // place.
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(unusable("must not carry a query string or fragment"));
    }
    // Credentials would be copied verbatim into latest.json, an object served
    // to the entire fleet and cached at every tier.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(unusable("must not carry credentials"));
    }
    // Compared with trailing slashes stripped because `download_url` strips
    // them too, so that difference alone is not a divergence.
    if parsed.as_str().trim_end_matches('/') != base_url.trim_end_matches('/') {
        return Err(unusable(
            "is not in normalized form - give scheme, host, optional non-default port and path \
             only, with no redundant slashes, percent-escapes, spaces or uppercase host",
        ));
    }
    Ok(())
}

/// Derive `latest.json` for a coordinate, or `Ok(None)` when no release is
/// available to every client.
///
/// `rollout_pct` is deliberately not honoured per-client here: `latest.json`
/// is one stored object shared by the whole fleet, so it can only describe a
/// release at 100%. That is the safe direction - a staged release can never
/// leak to every client through this file.
///
/// UNRESOLVED, and deliberately not papered over: how a staged macOS release
/// reaches the clients that ARE eligible. Spec 6 has the darwin route serve
/// this file "verbatim", while spec 8's read path says the handler evaluates
/// rollout against `cid`/`uid` - but `LatestManifest` carries no
/// `rollout_pct`, so there is nothing in these bytes to evaluate, and a
/// release below 100% is therefore delivered to nobody. Closing that needs a
/// storage/serving decision (serve a manifest derived per request from
/// `index.json`, or precompute one manifest per release and select among
/// them), which is a spec change rather than something this function can fix.
///
/// `url` points at tanukistore's own download route rather than a presigned
/// MinIO URL (spec 4.7), which is what keeps these bytes static and lets the
/// server return them without re-serializing.
pub fn derive_latest(
    index: &Index,
    coord: &Coordinate,
    base_url: &str,
) -> Result<Option<Vec<u8>>, DeriveError> {
    validate_base_url(base_url)?;
    let Some(release) = resolve::resolve_eligible(index, coord.app(), coord.channel(), None) else {
        return Ok(None);
    };
    let asset = pick(release, AssetKind::Zip)?;
    let manifest = LatestManifest {
        url: coord.download_url(base_url, &release.version, &asset.filename),
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

fn pick(release: &Release, kind: AssetKind) -> Result<&crate::model::Asset, DeriveError> {
    release
        .assets
        .iter()
        .find(|asset| asset.kind == kind)
        .ok_or_else(|| DeriveError::NoAsset {
            version: release.version.to_string(),
            kind,
        })
}

/// Derive the Squirrel.Windows `RELEASES` manifest: one
/// `{sha1} {filename} {size}` line per nupkg asset in the index, ascending by
/// version.
///
/// **Full-vs-delta is a caller-enforced obligation this function cannot
/// check.** Spec 5 says `RELEASES` lists every *full* nupkg, but `AssetKind`
/// has no full/delta distinction, so a delta nupkg recorded in `index.json`
/// is emitted here as an ordinary, unmarked line. Keeping deltas out of the
/// index (or out of the nupkg kind) is the publisher's job under spec 4.5;
/// nothing below detects a violation.
///
/// Unlike `derive_latest`, this walks the ENTIRE index. Squirrel.Windows
/// compares versions itself and needs a line for whatever version the client
/// is currently running; emitting only the newest yields a feed that works
/// for recent clients and silently fails for older ones.
///
/// Rollout percentages are ignored for the same reason — withholding a line
/// cannot stage a rollout on Windows, it only breaks clients on that version.
/// Windows rollout staging is out of scope for v1.
///
/// Filenames are relative, resolved by the client against the feed base URL.
pub fn derive_releases(index: &Index) -> Result<Vec<u8>, DeriveError> {
    let mut releases: Vec<&Release> = index.releases.iter().collect();
    releases.sort_by(|a, b| a.version.cmp(&b.version));

    let mut out = String::new();
    let mut previous: Option<&Release> = None;
    for release in releases {
        // Equal versions are adjacent after the sort. Two entries sharing a
        // version emit two lines with the same version and conflicting
        // SHA-1/size; Squirrel.Windows checksum-verifies whichever it picked,
        // fails, deletes the package and re-downloads forever. Duplicate
        // input is reachable — a hand edit, a backup restore, a concurrent
        // --force — which is exactly the class of out-of-band write spec 4.3
        // says the design must catch. Fail loudly at publish time instead.
        if let Some(previous) = previous
            && previous.version == release.version
        {
            return Err(DeriveError::DuplicateVersion {
                version: release.version.to_string(),
            });
        }
        previous = Some(release);

        // Squirrel.Windows compares versions against RELEASES itself and never
        // consults a server-side eligibility check, so a line in this file is
        // offered to EVERY Windows client. Withholding the line cannot stage a
        // rollout either - it only breaks clients already on that version. So a
        // rollout_pct below 100 here has exactly one honest outcome: refuse it,
        // rather than silently serving a 10% canary to the whole fleet. Spec 5
        // puts Windows staging out of scope for v1; this is what enforcing that
        // looks like instead of documenting it and hoping. index.json is
        // per-platform (spec 5), so darwin can still stage independently.
        if release.rollout_pct.get() < 100 {
            return Err(DeriveError::StagedRolloutUnsupportedOnWindows {
                version: release.version.to_string(),
                rollout_pct: release.rollout_pct.get(),
            });
        }

        let asset = pick(release, AssetKind::Nupkg)?;
        // Squirrel.Windows' entry parser matches the filename field as (\S+)
        // and THROWS on a line it cannot match, and that propagates out of
        // parsing the whole file. So one filename containing a space breaks
        // the entire feed for every version and every Windows client, and a
        // newline would inject a fabricated entry. Spec 4.5 assigns filename
        // validation to the publisher, but this is the last pure gate before
        // bytes reach the wire.
        if asset.filename.chars().any(char::is_whitespace) {
            return Err(DeriveError::FilenameHasWhitespace {
                filename: asset.filename.clone(),
            });
        }
        if asset.sha1.len() != 40 || !asset.sha1.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DeriveError::MalformedSha1 {
                filename: asset.filename.clone(),
                sha1: asset.sha1.clone(),
            });
        }

        out.push_str(&format!(
            "{} {} {}\n",
            asset.sha1, asset.filename, asset.size_bytes
        ));
    }
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arch, Asset, AssetKind, Index, Platform, Release, RolloutPct};
    use chrono::{TimeZone, Utc};
    use semver::Version;

    fn coord() -> Coordinate {
        Coordinate::new("myapp", "stable", Platform::Darwin, Arch::Arm64).unwrap()
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
    fn matches_the_golden_fixture_modulo_the_files_trailing_newline() {
        // latest-darwin-arm64.json ends with a newline because a text file in
        // a repo does; the manifest the server emits does not, and must not —
        // JSON has no trailing-newline convention and the stored bytes are
        // served verbatim. So the fixture is compared trim_end()-ed, and this
        // test is NOT byte-for-byte on the file. Its sibling for RELEASES is,
        // because there a trailing newline terminates the last entry and IS
        // part of the format.
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
        // Assert the whole URL, not just the absence of "com//": a
        // substring-absence assertion passes on URLs that are broken in every
        // other way, which is how raw, unencoded query values survived here.
        let index = Index { releases: vec![zip_release("1.5.0", 100)] };
        let bytes = derive_latest(&index, &coord(), "https://updates.example.com/")
            .unwrap()
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            manifest["url"],
            serde_json::json!(
                "https://updates.example.com/download/myapp/1.5.0\
                 ?platform=darwin&arch=arm64&channel=stable\
                 &filename=myapp-1.5.0-darwin-arm64.zip"
            )
        );
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
        assert_eq!(
            derive_latest(&index, &coord(), "https://updates.example.com"),
            Err(DeriveError::NoAsset {
                version: "1.5.0".to_owned(),
                kind: AssetKind::Zip,
            })
        );
    }

    #[test]
    fn a_base_url_that_cannot_be_joined_is_an_error() {
        // Spec 4.7: Squirrel.Mac hands `url` to NSURLSession, so it must be
        // absolute. Each group below is a distinct way the join breaks, and
        // every one of them used to pass some earlier, cheaper check.
        let index = Index {
            releases: vec![zip_release("1.5.0", 100)],
        };
        let rejects = |base: &str, reason: &'static str| {
            assert_eq!(
                derive_latest(&index, &coord(), base),
                Err(DeriveError::UnusableBaseUrl {
                    base_url: base.to_owned(),
                    reason,
                }),
                "base_url {base:?}"
            );
        };

        // No usable scheme at all.
        for base in ["", "updates.example.com", "/download", "ftp://x.example"] {
            rejects(base, "must start with http:// or https://");
        }
        // A scheme prefix alone is not enough - these pass `starts_with`.
        for base in ["https://", "http://"] {
            rejects(base, "has a scheme but no host");
        }
        // A NON-EMPTY authority is not enough either: `https://:443` has a
        // non-empty first slash-delimited substring, so it survived the
        // previous "first segment is non-empty" check while having no host.
        rejects("https://:443", "has a scheme but no host");
        for base in [
            "https://updates.example.com:bad",
            "https://updates.example.com:99999",
        ] {
            rejects(base, "has an invalid port");
        }
        for base in [
            "https://updates.example.com/?x=1",
            "https://updates.example.com#frag",
        ] {
            rejects(base, "must not carry a query string or fragment");
        }
        // latest.json is served to the whole fleet and cached at every tier.
        rejects(
            "https://user:pw@updates.example.com",
            "must not carry credentials",
        );

        let not_normalized = "is not in normalized form - give scheme, host, optional non-default \
                              port and path only, with no redundant slashes, percent-escapes, \
                              spaces or uppercase host";
        for base in [
            // Parses as host `download` - the URL grammar collapses the extra
            // slash - so a parse-only check would accept an obvious typo.
            "https:///download",
            // Parses only by percent-encoding the space; emitted raw, that
            // space makes NSURL(string:) return nil and updates stop silently.
            "https://updates.example.com/a b",
            "HTTPS://Updates.Example.COM",
            "https://ex%61mple.com",
        ] {
            rejects(base, not_normalized);
        }

        // The forms an operator actually configures, including a subpath, an
        // explicit non-default port and an IPv6 literal.
        for base in [
            "http://updates.example.com",
            "https://updates.example.com",
            "https://updates.example.com/",
            "https://updates.example.com/base",
            "https://updates.example.com:8443",
            "https://[::1]:8080",
        ] {
            assert!(
                derive_latest(&index, &coord(), base).is_ok(),
                "base_url {base:?}"
            );
        }
    }

    fn nupkg_release(version: &str, sha1: &str, size: u64) -> Release {
        Release {
            version: Version::parse(version).unwrap(),
            notes: String::new(),
            pub_date: Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap(),
            rollout_pct: RolloutPct::FULL,
            assets: vec![Asset {
                kind: AssetKind::Nupkg,
                filename: format!("myapp-{version}-full.nupkg"),
                sha1: sha1.to_owned(),
                sha512: "cf83e1357eefb8bd".to_owned(),
                size_bytes: size,
            }],
        }
    }

    #[test]
    fn releases_matches_the_golden_fixture_byte_for_byte() {
        let index = Index {
            releases: vec![
                nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 94_371_840),
                nupkg_release("1.4.0", "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d", 93_323_264),
            ],
        };
        let bytes = derive_releases(&index).unwrap();
        let expected = include_str!("../tests/fixtures/RELEASES-win32-x64");
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }

    #[test]
    fn releases_lists_the_whole_history_ascending() {
        let index = Index {
            releases: vec![
                nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 3),
                nupkg_release("1.4.0", "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d", 2),
                nupkg_release("1.3.0", "0beec7b5ea3f0fdbc95d0dd47f3c5bc275da8a33", 1),
            ],
        };
        let text = String::from_utf8(derive_releases(&index).unwrap()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "every version must appear: {text}");
        assert!(lines[0].contains("1.3.0"), "ascending order: {text}");
        assert!(lines[2].contains("1.5.0"), "ascending order: {text}");
    }

    #[test]
    fn releases_rejects_a_partial_rollout_because_windows_cannot_stage() {
        // A line in RELEASES reaches every Windows client, and withholding it
        // would break clients already on that version - so neither emitting
        // nor omitting implements a rollout. Refusing is the only honest
        // option; the alternative ships a 10% canary to 100% of the fleet.
        let mut staged = nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 3);
        staged.rollout_pct = RolloutPct::new(10).unwrap();
        let index = Index { releases: vec![staged] };
        assert_eq!(
            derive_releases(&index),
            Err(DeriveError::StagedRolloutUnsupportedOnWindows {
                version: "1.5.0".to_owned(),
                rollout_pct: 10,
            })
        );
    }

    #[test]
    fn releases_accepts_a_full_rollout() {
        let index = Index {
            releases: vec![nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 3)],
        };
        let text = String::from_utf8(derive_releases(&index).unwrap()).unwrap();
        assert!(text.contains("1.5.0"), "{text}");
    }

    #[test]
    fn releases_is_empty_for_an_empty_index() {
        let index = Index::default();
        assert!(derive_releases(&index).unwrap().is_empty());
    }

    #[test]
    fn releases_rejects_two_entries_sharing_a_version() {
        // Two lines for one version with conflicting SHA-1s makes
        // Squirrel.Windows checksum-fail, delete and re-download forever.
        let index = Index {
            releases: vec![
                nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 1),
                nupkg_release("1.5.0", "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d", 2),
            ],
        };
        assert_eq!(
            derive_releases(&index),
            Err(DeriveError::DuplicateVersion { version: "1.5.0".to_owned() })
        );
    }

    #[test]
    fn releases_rejects_a_filename_containing_whitespace() {
        // Squirrel.Windows' entry parser matches (\S+) and throws on a
        // non-matching line, taking the whole feed down with it.
        let mut release = nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 1);
        release.assets[0].filename = "My App-1.5.0-full.nupkg".to_owned();
        let index = Index { releases: vec![release] };
        assert_eq!(
            derive_releases(&index),
            Err(DeriveError::FilenameHasWhitespace {
                filename: "My App-1.5.0-full.nupkg".to_owned(),
            })
        );
    }

    #[test]
    fn releases_rejects_a_filename_containing_a_newline() {
        // A newline would inject a fabricated entry into the feed.
        let mut release = nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 1);
        release.assets[0].filename = "a.nupkg\nff 1".to_owned();
        let index = Index { releases: vec![release] };
        assert_eq!(
            derive_releases(&index),
            Err(DeriveError::FilenameHasWhitespace {
                filename: "a.nupkg\nff 1".to_owned(),
            })
        );
    }

    #[test]
    fn releases_rejects_a_sha1_that_is_not_40_hex_characters() {
        let index = Index { releases: vec![nupkg_release("1.5.0", "deadbeef", 1)] };
        assert_eq!(
            derive_releases(&index),
            Err(DeriveError::MalformedSha1 {
                filename: "myapp-1.5.0-full.nupkg".to_owned(),
                sha1: "deadbeef".to_owned(),
            })
        );

        // Right length, wrong alphabet.
        let index = Index {
            releases: vec![nupkg_release("1.5.0", &"z".repeat(40), 1)],
        };
        assert_eq!(
            derive_releases(&index),
            Err(DeriveError::MalformedSha1 {
                filename: "myapp-1.5.0-full.nupkg".to_owned(),
                sha1: "z".repeat(40),
            })
        );
    }

    #[test]
    fn releases_errors_when_a_release_has_no_nupkg() {
        let index = Index { releases: vec![zip_release("1.5.0", 100)] };
        assert_eq!(
            derive_releases(&index),
            Err(DeriveError::NoAsset {
                version: "1.5.0".to_owned(),
                kind: AssetKind::Nupkg,
            })
        );
    }
}
