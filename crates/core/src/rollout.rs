use sha2::{Digest, Sha256};

use crate::model::RolloutPct;

/// Deterministically bucket a client into `0..=99`.
///
/// FROZEN WIRE CONTRACT (spec 4.2). SHA-256 over `{app}:{channel}:{id}`,
/// first 8 bytes big-endian as u64, modulo 100. Unseeded on purpose: a
/// per-process seed (as in std's RandomState) would re-bucket clients on
/// every restart and disagree between replicas, so a client would flap
/// between old and new versions. Changing this function re-buckets the
/// entire fleet simultaneously.
pub fn bucket(app: &str, channel: &str, id: &str) -> u8 {
    let mut hasher = Sha256::new();
    hasher.update(app.as_bytes());
    hasher.update(b":");
    hasher.update(channel.as_bytes());
    hasher.update(b":");
    hasher.update(id.as_bytes());
    let digest = hasher.finalize();
    let head: [u8; 8] = digest[..8].try_into().expect("sha256 yields 32 bytes");
    (u64::from_be_bytes(head) % 100) as u8
}

/// Whether a client is eligible for a release at `pct`.
///
/// `id` is the already-resolved bucketing identity: `cid` when present,
/// else `uid`, else `None`. A client with no stable identity is ineligible
/// for any partial rollout — failing closed, because an erroneous "no
/// update" costs one polling interval while an erroneous "update" ships a
/// build that cannot be un-shipped.
pub fn eligible(app: &str, channel: &str, id: Option<&str>, pct: RolloutPct) -> bool {
    if pct.get() >= 100 {
        return true;
    }
    match id {
        Some(id) => bucket(app, channel, id) < pct.get(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RolloutPct;

    #[test]
    fn bucket_is_stable_and_in_range() {
        let first = bucket("myapp", "stable", "install-abc");
        for _ in 0..100 {
            assert_eq!(bucket("myapp", "stable", "install-abc"), first);
        }
        assert!(first < 100);
    }

    #[test]
    fn bucket_is_pinned_to_known_values() {
        // Frozen wire contract (spec 4.2). If these change, the entire fleet
        // re-buckets at once. Changing them is never a routine edit.
        assert_eq!(bucket("myapp", "stable", "install-abc"), 18);
        assert_eq!(bucket("myapp", "beta", "install-abc"), 91);
        assert_eq!(bucket("otherapp", "stable", "install-abc"), 91);
    }

    #[test]
    fn bucket_distributes_roughly_evenly() {
        let mut under_10 = 0usize;
        let total = 10_000usize;
        for i in 0..total {
            if bucket("myapp", "stable", &format!("install-{i}")) < 10 {
                under_10 += 1;
            }
        }
        // The real value for this seed set is 1034. Bounds are generous
        // because this asserts "not pathological", not a precise figure.
        assert!(
            (850..1150).contains(&under_10),
            "expected ~1000 of {total} under 10, got {under_10}"
        );
    }

    #[test]
    fn full_rollout_is_eligible_even_without_an_id() {
        assert!(eligible("myapp", "stable", None, RolloutPct::FULL));
        assert!(eligible("myapp", "stable", Some("x"), RolloutPct::FULL));
    }

    #[test]
    fn partial_rollout_excludes_clients_with_no_id() {
        let half = RolloutPct::new(50).unwrap();
        assert!(!eligible("myapp", "stable", None, half));
    }

    #[test]
    fn zero_rollout_is_never_eligible() {
        let zero = RolloutPct::new(0).unwrap();
        assert!(!eligible("myapp", "stable", Some("install-abc"), zero));
        assert!(!eligible("myapp", "stable", None, zero));
    }

    #[test]
    fn partial_rollout_tracks_the_bucket() {
        let id = "install-abc";
        let b = bucket("myapp", "stable", id); // 18
        let just_over = RolloutPct::new(b + 1).unwrap();
        let exactly_at = RolloutPct::new(b).unwrap();
        assert!(eligible("myapp", "stable", Some(id), just_over));
        assert!(!eligible("myapp", "stable", Some(id), exactly_at));
    }
}
