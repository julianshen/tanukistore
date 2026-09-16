//! Read-path protection for the origin (spec 9.2), as an `ObjectStore`
//! decorator so the cache and handlers never know it exists.
//!
//! Order per call: breaker check -> bulkhead permit -> timeout -> origin.
//! The bulkhead bounds load BEFORE anything fails; the breaker removes load
//! once failure is sustained. A breaker alone would still let a thundering herd
//! pile onto a MinIO that is merely slow, because slow is not yet failed.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::store::{Object, ObjectStore, PutCondition, StoreError};

#[derive(Debug, Clone)]
pub struct BreakerConfig {
    /// Rolling window over which the failure ratio is computed.
    pub window: Duration,
    /// Below this many requests in the window the ratio is not trusted.
    pub min_requests: usize,
    /// Trip when failures / requests reaches this.
    pub failure_ratio: f64,
    pub cooldown: Duration,
    pub max_cooldown: Duration,
    pub half_open_trials: u32,
    /// Concurrent origin calls allowed per process (the bulkhead).
    pub max_concurrency: usize,
    /// Per-operation deadline. A hung connection never returns an error, so
    /// without this the breaker would never see a failure to count.
    pub timeout: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        BreakerConfig {
            window: Duration::from_secs(10),
            min_requests: 20,
            failure_ratio: 0.5,
            cooldown: Duration::from_secs(5),
            max_cooldown: Duration::from_secs(300),
            half_open_trials: 3,
            max_concurrency: 32,
            timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug)]
enum State {
    Closed,
    Open { until: Instant, cooldown: Duration },
    HalfOpen { admitted: u32, cooldown: Duration },
}

#[derive(Debug, Clone, Copy)]
enum Admission {
    Normal,
    Trial,
}

struct Inner {
    state: State,
    outcomes: VecDeque<(Instant, bool)>,
}

pub struct CircuitBreakerStore<S> {
    origin: S,
    config: BreakerConfig,
    inner: Mutex<Inner>,
    permits: Semaphore,
}

impl<S: ObjectStore> CircuitBreakerStore<S> {
    pub fn new(origin: S, config: BreakerConfig) -> Self {
        CircuitBreakerStore {
            permits: Semaphore::new(config.max_concurrency),
            origin,
            config,
            inner: Mutex::new(Inner {
                state: State::Closed,
                outcomes: VecDeque::new(),
            }),
        }
    }

    pub fn state(&self) -> BreakerState {
        match self.inner.lock().unwrap().state {
            State::Closed => BreakerState::Closed,
            State::Open { .. } => BreakerState::Open,
            State::HalfOpen { .. } => BreakerState::HalfOpen,
        }
    }

    fn admit(&self) -> Result<Admission, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        match inner.state {
            State::Closed => Ok(Admission::Normal),
            State::Open { until, cooldown } if now >= until => {
                inner.state = State::HalfOpen {
                    admitted: 1,
                    cooldown,
                };
                Ok(Admission::Trial)
            }
            State::Open { .. } => Err(self.reject()),
            State::HalfOpen {
                ref mut admitted, ..
            } if *admitted < self.config.half_open_trials => {
                *admitted += 1;
                Ok(Admission::Trial)
            }
            State::HalfOpen { .. } => Err(self.reject()),
        }
    }

    fn reject(&self) -> StoreError {
        metrics::counter!("circuit_breaker_rejected_total").increment(1);
        StoreError::BreakerOpen
    }

    fn record(&self, admission: Admission, failed: bool) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        match admission {
            Admission::Trial => match (&inner.state, failed) {
                // One failed trial is enough: reopen with a longer cooldown so a
                // backend that keeps failing is probed less and less often.
                (State::HalfOpen { cooldown, .. }, true) => {
                    let cooldown = (*cooldown * 2).min(self.config.max_cooldown);
                    inner.state = State::Open {
                        until: now + cooldown,
                        cooldown,
                    };
                    tracing::warn!(?cooldown, "circuit breaker reopened after failed trial");
                }
                (State::HalfOpen { .. }, false) => {
                    inner.state = State::Closed;
                    inner.outcomes.clear();
                    tracing::info!("circuit breaker closed");
                }
                // Another trial already decided the outcome.
                _ => {}
            },
            Admission::Normal => {
                inner.outcomes.push_back((now, failed));
                let horizon = now.checked_sub(self.config.window).unwrap_or(now);
                while inner.outcomes.front().is_some_and(|(at, _)| *at < horizon) {
                    inner.outcomes.pop_front();
                }
                let total = inner.outcomes.len();
                let failures = inner.outcomes.iter().filter(|(_, f)| *f).count();
                if matches!(inner.state, State::Closed)
                    && total >= self.config.min_requests
                    && failures as f64 >= self.config.failure_ratio * total as f64
                {
                    inner.state = State::Open {
                        until: now + self.config.cooldown,
                        cooldown: self.config.cooldown,
                    };
                    tracing::warn!(failures, total, "circuit breaker opened");
                }
            }
        }
    }

    async fn guarded<T, F>(&self, op: F) -> Result<T, StoreError>
    where
        F: Future<Output = Result<T, StoreError>>,
    {
        let admission = self.admit()?;
        let timeout = self.config.timeout;
        // The deadline covers the wait for a permit too. Otherwise a saturated
        // bulkhead is an unbounded queue, and a slow origin turns into requests
        // that hang forever instead of failing and tripping the breaker.
        let result = tokio::time::timeout(timeout, async {
            let _permit = self
                .permits
                .acquire()
                .await
                .expect("the semaphore is never closed");
            op.await
        })
        .await
        .unwrap_or(Err(StoreError::Timeout(timeout)));
        self.record(
            admission,
            matches!(&result, Err(e) if e.is_backend_failure()),
        );
        result
    }
}

#[async_trait]
impl<S: ObjectStore> ObjectStore for CircuitBreakerStore<S> {
    async fn get(&self, key: &str) -> Result<Object, StoreError> {
        self.guarded(self.origin.get(key)).await
    }

    async fn head(&self, key: &str) -> Result<u64, StoreError> {
        self.guarded(self.origin.head(key)).await
    }

    // Writes pass straight through. Only the server wraps its store in a
    // breaker (spec 9.2), and the server never writes; the publisher retries
    // instead. A 2s deadline would also make every asset upload fail.
    async fn put(
        &self,
        key: &str,
        body: Bytes,
        content_type: &str,
        condition: PutCondition,
    ) -> Result<Option<String>, StoreError> {
        self.origin.put(key, body, content_type, condition).await
    }

    async fn put_file(&self, key: &str, path: &Path, content_type: &str) -> Result<(), StoreError> {
        self.origin.put_file(key, path, content_type).await
    }

    // Local signing, never gated (spec 9.2).
    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, StoreError> {
        self.origin.presign_get(key, ttl).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::InMemoryStore;

    fn breaker(origin: Arc<InMemoryStore>) -> CircuitBreakerStore<Arc<InMemoryStore>> {
        CircuitBreakerStore::new(origin, BreakerConfig::default())
    }

    #[tokio::test(start_paused = true)]
    async fn opens_on_a_failure_ratio_then_fails_fast_without_touching_the_origin() {
        let origin = Arc::new(InMemoryStore::new());
        origin.insert("k", "v");
        let store = breaker(origin.clone());

        origin.set_failing(true);
        // 19 failures is below the minimum sample, so it must not trip yet.
        for _ in 0..19 {
            assert!(store.get("k").await.is_err());
        }
        assert_eq!(store.state(), BreakerState::Closed);
        assert!(store.get("k").await.is_err());
        assert_eq!(store.state(), BreakerState::Open);

        let calls = origin.get_count();
        assert_eq!(store.get("k").await, Err(StoreError::BreakerOpen));
        assert_eq!(
            origin.get_count(),
            calls,
            "an open breaker must not call the origin"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn trips_on_partial_degradation_not_just_consecutive_failures() {
        let origin = Arc::new(InMemoryStore::new());
        origin.insert("k", "v");
        let store = breaker(origin.clone());
        // Alternating success and failure never produces a long consecutive run,
        // but it is a 50% failure rate - exactly what a ratio exists to catch.
        for i in 0..20 {
            origin.set_failing(i % 2 == 0);
            let _ = store.get("k").await;
        }
        assert_eq!(store.state(), BreakerState::Open);
    }

    #[tokio::test(start_paused = true)]
    async fn not_found_never_trips_the_breaker() {
        let origin = Arc::new(InMemoryStore::new());
        let store = breaker(origin);
        for _ in 0..100 {
            assert!(matches!(
                store.get("unpublished/channel/index.json").await,
                Err(StoreError::NotFound(_))
            ));
        }
        assert_eq!(store.state(), BreakerState::Closed);
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_closes_on_success_and_reopens_longer_on_failure() {
        let origin = Arc::new(InMemoryStore::new());
        origin.insert("k", "v");
        let store = breaker(origin.clone());
        origin.set_failing(true);
        for _ in 0..20 {
            let _ = store.get("k").await;
        }
        assert_eq!(store.state(), BreakerState::Open);

        // After the cooldown a trial is admitted; it fails and the breaker
        // reopens with a DOUBLED cooldown, so 5s later it is still open.
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(matches!(store.get("k").await, Err(StoreError::Backend(_))));
        assert_eq!(store.state(), BreakerState::Open);
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(store.get("k").await, Err(StoreError::BreakerOpen));

        // Past the doubled cooldown, a successful trial closes it.
        tokio::time::advance(Duration::from_secs(5)).await;
        origin.set_failing(false);
        assert!(store.get("k").await.is_ok());
        assert_eq!(store.state(), BreakerState::Closed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_origin_times_out_and_counts_as_a_failure() {
        let origin = Arc::new(InMemoryStore::new());
        origin.insert("k", "v");
        origin.set_delay(Duration::from_secs(60));
        let store = Arc::new(breaker(origin));
        // Concurrent, as a real fleet is. Sequential 2s timeouts would spread 20
        // samples over 40s, and a 10s window would never hold enough of them.
        let calls: Vec<_> = (0..20)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move { store.get("k").await })
            })
            .collect();
        for call in calls {
            assert_eq!(
                call.await.unwrap(),
                Err(StoreError::Timeout(Duration::from_secs(2)))
            );
        }
        assert_eq!(
            store.state(),
            BreakerState::Open,
            "without timeouts counting, a hung origin never trips the breaker"
        );
    }
}
