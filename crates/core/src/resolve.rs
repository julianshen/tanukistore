use crate::model::{Index, Release};
use crate::rollout;

/// Highest-version release this client is eligible for, or `None`.
///
/// Highest-version rather than last-appended: `index.json` is append-only,
/// so publishing a 1.4.1 hotfix after 1.5.0 must not downgrade the fleet.
/// semver ordering also puts `1.5.0-beta.1` below `1.5.0`, which is what a
/// channel serving both should do.
pub fn resolve_eligible<'a>(
    index: &'a Index,
    app: &str,
    channel: &str,
    id: Option<&str>,
) -> Option<&'a Release> {
    index
        .releases
        .iter()
        .filter(|release| rollout::eligible(app, channel, id, release.rollout_pct))
        .max_by(|a, b| a.version.cmp(&b.version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Index, Release, RolloutPct};
    use chrono::{TimeZone, Utc};
    use semver::Version;

    fn release(version: &str, pct: u8) -> Release {
        Release {
            version: Version::parse(version).unwrap(),
            notes: String::new(),
            pub_date: Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap(),
            rollout_pct: RolloutPct::new(pct).unwrap(),
            assets: vec![],
        }
    }

    #[test]
    fn empty_index_resolves_to_nothing() {
        let index = Index::default();
        assert!(resolve_eligible(&index, "myapp", "stable", Some("i")).is_none());
    }

    #[test]
    fn picks_highest_version_not_last_appended() {
        // 1.4.1 appended after 1.5.0 must not win.
        let index = Index {
            releases: vec![release("1.5.0", 100), release("1.4.1", 100)],
        };
        let got = resolve_eligible(&index, "myapp", "stable", None).unwrap();
        assert_eq!(got.version, Version::new(1, 5, 0));
    }

    #[test]
    fn skips_releases_the_client_is_not_eligible_for() {
        // 1.5.0 at 0% is never eligible; 1.4.0 at 100% always is.
        let index = Index {
            releases: vec![release("1.4.0", 100), release("1.5.0", 0)],
        };
        let got = resolve_eligible(&index, "myapp", "stable", Some("i")).unwrap();
        assert_eq!(got.version, Version::new(1, 4, 0));
    }

    #[test]
    fn resolves_to_nothing_when_no_release_is_eligible() {
        let index = Index { releases: vec![release("1.5.0", 0)] };
        assert!(resolve_eligible(&index, "myapp", "stable", Some("i")).is_none());
    }

    #[test]
    fn build_metadata_does_not_affect_ordering() {
        // Per semver, build metadata is ignored in comparison, so two
        // releases differing only by +build are ordered arbitrarily. The
        // publisher must never rely on it to distinguish releases.
        let a = release("1.5.0+build.1", 100);
        let b = release("1.5.0+build.2", 100);
        let index = Index { releases: vec![a, b] };
        let got = resolve_eligible(&index, "myapp", "stable", None).unwrap();
        assert_eq!(got.version.major, 1);
        assert_eq!(got.version.minor, 5);
    }

    #[test]
    fn prerelease_loses_to_its_stable_counterpart() {
        let index = Index {
            releases: vec![release("1.5.0-beta.1", 100), release("1.5.0", 100)],
        };
        let got = resolve_eligible(&index, "myapp", "stable", None).unwrap();
        assert_eq!(got.version, Version::new(1, 5, 0));
    }
}
