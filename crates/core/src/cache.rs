//! L1 manifest cache (spec 9.1): freshness window, single-flight, and
//! stale-on-error.
//!
//! Plain TTL expiry would turn a MinIO hiccup into a fleet-wide failure,
//! because an expired entry has nowhere to go. Here the TTL is only a long
//! max-staleness bound. Past the short freshness window a read attempts ONE
//! refresh, and if that refresh fails the old bytes are served instead of an
//! error.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use semver::Version;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::keys::KeyKind;
use crate::model::{AppConfig, Index};
use crate::store::{ObjectStore, StoreError};

/// A manifest parsed once at fetch time, so a cache hit costs no JSON work.
#[derive(Debug)]
pub enum Parsed {
    Config(AppConfig),
    Index(Index),
    /// `latest.json`, reduced to the one field the darwin handler compares.
    Latest(Version),
    /// `RELEASES` is served verbatim and never inspected.
    Releases,
}

#[derive(Debug)]
pub struct CachedObject {
    /// `None` records a confirmed 404. Caching absence matters as much as
    /// caching presence: a fleet polling a channel that has not published yet
    /// would otherwise send every poll straight to MinIO.
    pub body: Option<(Bytes, Parsed)>,
    pub fetched_at: Instant,
}

#[derive(Debug)]
pub struct Lookup {
    pub object: Arc<CachedObject>,
    /// True when a refresh failed and these bytes are older than the freshness
    /// window. The response is still correct enough to serve; callers mark the
    /// request degraded.
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheError {
    /// The origin failed and there is no copy at all to fall back on.
    #[error("origin unavailable and nothing cached: {0}")]
    Unavailable(StoreError),
    /// The origin returned bytes that do not parse. Not cached: a corrupt
    /// object is an out-of-band write that must stay loud until it is fixed.
    #[error("{key} is corrupt: {reason}")]
    Corrupt { key: String, reason: String },
}

#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Freshness for `index.json`, `latest.json` and `RELEASES`.
    pub manifest_freshness: Duration,
    /// Freshness for `config.json`, which changes only when a channel is added.
    pub config_freshness: Duration,
    /// Hard upper bound on how stale anything served can get.
    pub max_staleness: Duration,
    pub max_entries: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            manifest_freshness: Duration::from_secs(30),
            config_freshness: Duration::from_secs(300),
            max_staleness: Duration::from_secs(3600),
            max_entries: 100_000,
        }
    }
}

pub struct ManifestCache<S> {
    store: S,
    config: CacheConfig,
    entries: moka::future::Cache<String, Arc<CachedObject>>,
    refreshing: moka::future::Cache<String, Arc<Mutex<()>>>,
}

#[derive(Deserialize)]
struct LatestName {
    name: String,
}

impl<S: ObjectStore> ManifestCache<S> {
    pub fn new(store: S, config: CacheConfig) -> Self {
        ManifestCache {
            entries: moka::future::Cache::builder()
                .max_capacity(config.max_entries)
                .time_to_live(config.max_staleness)
                .build(),
            refreshing: moka::future::Cache::builder()
                .max_capacity(config.max_entries)
                .time_to_idle(Duration::from_secs(600))
                .build(),
            store,
            config,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub async fn get(&self, key: &str, kind: KeyKind) -> Result<Lookup, CacheError> {
        if let Some(object) = self.entries.get(key).await {
            // Enforced here rather than trusted to moka's TTL, which runs on its
            // own clock: this is the bound that stops an outage from serving
            // hour-old bytes indefinitely, so it has to be exact and testable.
            if object.fetched_at.elapsed() >= self.config.max_staleness {
                self.entries.invalidate(key).await;
                return self.get_cold(key, kind).await;
            }
            if self.is_fresh(&object, kind) {
                return Ok(Lookup {
                    object,
                    stale: false,
                });
            }
            return self.refresh(key, kind, object).await;
        }
        self.get_cold(key, kind).await
    }

    async fn get_cold(&self, key: &str, kind: KeyKind) -> Result<Lookup, CacheError> {
        // A cold miss is single-flighted by moka: concurrent callers for the
        // same key share one origin fetch rather than stampeding MinIO.
        let object = self
            .entries
            .try_get_with(key.to_owned(), self.fetch(key, kind))
            .await
            .map_err(|e| (*e).clone())?;
        Ok(Lookup {
            object,
            stale: false,
        })
    }

    /// Drops an entry outright, bypassing the freshness window. For
    /// invalidation on publish.
    pub async fn evict(&self, key: &str) {
        self.entries.invalidate(key).await;
    }

    fn is_fresh(&self, object: &CachedObject, kind: KeyKind) -> bool {
        let window = match kind {
            KeyKind::Config => self.config.config_freshness,
            _ => self.config.manifest_freshness,
        };
        object.fetched_at.elapsed() < window
    }

    async fn refresh(
        &self,
        key: &str,
        kind: KeyKind,
        held: Arc<CachedObject>,
    ) -> Result<Lookup, CacheError> {
        let lock = self
            .refreshing
            .get_with(key.to_owned(), async { Arc::new(Mutex::new(())) })
            .await;
        // Exactly one request refreshes. Everyone else arriving meanwhile gets
        // the held copy immediately rather than queueing behind the refresh: it
        // is at most one origin round-trip past its window, and making them
        // wait would put the origin's latency back on the hot path.
        let Ok(_guard) = lock.try_lock() else {
            return Ok(Lookup {
                object: held,
                stale: false,
            });
        };
        match self.fetch(key, kind).await {
            Ok(object) => {
                self.entries.insert(key.to_owned(), object.clone()).await;
                Ok(Lookup {
                    object,
                    stale: false,
                })
            }
            Err(error) => {
                metrics::counter!("manifest_stale_served_total", "key_kind" => kind.to_string())
                    .increment(1);
                tracing::warn!(key, %error, "refresh failed, serving stale manifest");
                Ok(Lookup {
                    object: held,
                    stale: true,
                })
            }
        }
    }

    async fn fetch(&self, key: &str, kind: KeyKind) -> Result<Arc<CachedObject>, CacheError> {
        let body = match self.store.get(key).await {
            Ok(object) => {
                let parsed = parse(key, kind, &object.bytes)?;
                Some((object.bytes, parsed))
            }
            Err(StoreError::NotFound(_)) => None,
            Err(other) => return Err(CacheError::Unavailable(other)),
        };
        Ok(Arc::new(CachedObject {
            body,
            fetched_at: Instant::now(),
        }))
    }
}

fn parse(key: &str, kind: KeyKind, bytes: &[u8]) -> Result<Parsed, CacheError> {
    let corrupt = |reason: String| CacheError::Corrupt {
        key: key.to_owned(),
        reason,
    };
    Ok(match kind {
        KeyKind::Config => {
            Parsed::Config(serde_json::from_slice(bytes).map_err(|e| corrupt(e.to_string()))?)
        }
        KeyKind::Index => {
            Parsed::Index(serde_json::from_slice(bytes).map_err(|e| corrupt(e.to_string()))?)
        }
        KeyKind::Latest => {
            let latest: LatestName =
                serde_json::from_slice(bytes).map_err(|e| corrupt(e.to_string()))?;
            Parsed::Latest(Version::parse(&latest.name).map_err(|e| corrupt(e.to_string()))?)
        }
        KeyKind::Releases => Parsed::Releases,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::InMemoryStore;

    const INDEX: &str = "[]";

    fn cache(store: Arc<InMemoryStore>) -> ManifestCache<Arc<InMemoryStore>> {
        ManifestCache::new(store, CacheConfig::default())
    }

    #[tokio::test(start_paused = true)]
    async fn a_fresh_hit_does_not_touch_the_origin() {
        let store = Arc::new(InMemoryStore::new());
        store.insert("a/index.json", INDEX);
        let cache = cache(store.clone());
        cache.get("a/index.json", KeyKind::Index).await.unwrap();
        cache.get("a/index.json", KeyKind::Index).await.unwrap();
        assert_eq!(store.get_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn absence_is_cached_too() {
        let store = Arc::new(InMemoryStore::new());
        let cache = cache(store.clone());
        for _ in 0..10 {
            let lookup = cache.get("a/latest.json", KeyKind::Latest).await.unwrap();
            assert!(lookup.object.body.is_none());
        }
        assert_eq!(
            store.get_count(),
            1,
            "polling an unpublished feed must not hammer the origin"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn past_the_freshness_window_it_refreshes() {
        let store = Arc::new(InMemoryStore::new());
        store.insert("a/latest.json", r#"{"name":"1.0.0"}"#);
        let cache = cache(store.clone());
        cache.get("a/latest.json", KeyKind::Latest).await.unwrap();

        store.insert("a/latest.json", r#"{"name":"1.1.0"}"#);
        tokio::time::advance(Duration::from_secs(31)).await;
        let lookup = cache.get("a/latest.json", KeyKind::Latest).await.unwrap();
        assert!(matches!(
            &lookup.object.body,
            Some((_, Parsed::Latest(v))) if *v == Version::new(1, 1, 0)
        ));
        assert!(!lookup.stale);
    }

    #[tokio::test(start_paused = true)]
    async fn an_origin_outage_serves_stale_instead_of_failing() {
        let store = Arc::new(InMemoryStore::new());
        store.insert("a/latest.json", r#"{"name":"1.0.0"}"#);
        let cache = cache(store.clone());
        cache.get("a/latest.json", KeyKind::Latest).await.unwrap();

        store.set_failing(true);
        tokio::time::advance(Duration::from_secs(31)).await;
        let lookup = cache.get("a/latest.json", KeyKind::Latest).await.unwrap();
        assert!(lookup.stale);
        assert!(matches!(
            &lookup.object.body,
            Some((_, Parsed::Latest(v))) if *v == Version::new(1, 0, 0)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn an_origin_outage_with_nothing_held_is_an_error() {
        let store = Arc::new(InMemoryStore::new());
        store.set_failing(true);
        let cache = cache(store);
        assert!(matches!(
            cache.get("a/latest.json", KeyKind::Latest).await,
            Err(CacheError::Unavailable(StoreError::Backend(_)))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_data_expires_at_the_max_staleness_bound() {
        let store = Arc::new(InMemoryStore::new());
        store.insert("a/latest.json", r#"{"name":"1.0.0"}"#);
        let cache = cache(store.clone());
        cache.get("a/latest.json", KeyKind::Latest).await.unwrap();

        store.set_failing(true);
        tokio::time::advance(Duration::from_secs(3601)).await;
        assert!(
            cache.get("a/latest.json", KeyKind::Latest).await.is_err(),
            "an outage longer than max-staleness must surface, not serve hour-old bytes forever"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_cold_misses_share_one_origin_fetch() {
        let store = Arc::new(InMemoryStore::new());
        store.insert("a/index.json", INDEX);
        store.set_delay(Duration::from_millis(100));
        let cache = Arc::new(cache(store.clone()));
        let tasks: Vec<_> = (0..50)
            .map(|_| {
                let cache = cache.clone();
                tokio::spawn(async move { cache.get("a/index.json", KeyKind::Index).await })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(store.get_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn corrupt_objects_are_reported_and_not_cached() {
        let store = Arc::new(InMemoryStore::new());
        store.insert("a/index.json", "{not json");
        let cache = cache(store.clone());
        assert!(matches!(
            cache.get("a/index.json", KeyKind::Index).await,
            Err(CacheError::Corrupt { .. })
        ));
        store.insert("a/index.json", INDEX);
        assert!(cache.get("a/index.json", KeyKind::Index).await.is_ok());
    }
}
