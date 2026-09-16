//! The object-storage boundary (spec 7). Everything above this trait - cache,
//! breaker, handlers, publisher - is written against `ObjectStore` and never
//! against the AWS SDK, which is what lets all of it be tested against
//! `InMemoryStore` with injected failures.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

/// One stored object and the version tag that conditional writes compare
/// against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub bytes: Bytes,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("precondition failed writing {0}")]
    PreconditionFailed(String),
    #[error("storage operation timed out after {0:?}")]
    Timeout(Duration),
    #[error("circuit breaker is open")]
    BreakerOpen,
    #[error("storage backend error: {0}")]
    Backend(String),
}

impl StoreError {
    /// Whether this outcome is evidence that the backend is unhealthy.
    ///
    /// `NotFound` and `PreconditionFailed` are NOT: both are the backend
    /// answering correctly. Spec 9.2 is explicit about 404 in particular - a
    /// fleet polling a channel that has not published yet generates a steady
    /// stream of them, and counting those would trip the breaker and take every
    /// other app's feed down with it.
    pub fn is_backend_failure(&self) -> bool {
        matches!(self, StoreError::Timeout(_) | StoreError::Backend(_))
    }
}

/// Write precondition for optimistic concurrency on `index.json` (spec 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutCondition {
    /// Unconditional overwrite.
    None,
    /// Write only if the stored object still carries this ETag.
    IfMatch(String),
    /// Write only if no object exists at the key yet.
    IfNoneMatch,
}

#[async_trait]
pub trait ObjectStore: Send + Sync + 'static {
    async fn get(&self, key: &str) -> Result<Object, StoreError>;

    /// Returns the new ETag when the backend reports one.
    async fn put(
        &self,
        key: &str,
        body: Bytes,
        content_type: &str,
        condition: PutCondition,
    ) -> Result<Option<String>, StoreError>;

    /// Streams a file from disk. Separate from `put` so a 200MB asset is never
    /// held in memory whole.
    async fn put_file(&self, key: &str, path: &Path, content_type: &str) -> Result<(), StoreError>;

    /// Size in bytes of the object at `key`.
    async fn head(&self, key: &str) -> Result<u64, StoreError>;

    /// A time-limited GET URL for `key`. Local signing only - spec 9.2 says it
    /// is never gated by the breaker, because it makes no network call.
    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, StoreError>;
}

#[async_trait]
impl<S: ObjectStore + ?Sized> ObjectStore for std::sync::Arc<S> {
    async fn get(&self, key: &str) -> Result<Object, StoreError> {
        (**self).get(key).await
    }
    async fn put(
        &self,
        key: &str,
        body: Bytes,
        content_type: &str,
        condition: PutCondition,
    ) -> Result<Option<String>, StoreError> {
        (**self).put(key, body, content_type, condition).await
    }
    async fn put_file(&self, key: &str, path: &Path, content_type: &str) -> Result<(), StoreError> {
        (**self).put_file(key, path, content_type).await
    }
    async fn head(&self, key: &str) -> Result<u64, StoreError> {
        (**self).head(key).await
    }
    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, StoreError> {
        (**self).presign_get(key, ttl).await
    }
}

/// Test fake with switchable failure modes.
#[derive(Default)]
pub struct InMemoryStore {
    objects: Mutex<HashMap<String, (Bytes, String)>>,
    next_etag: AtomicU64,
    failing: AtomicBool,
    delay_ms: AtomicU64,
    gets: AtomicU64,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// While set, every read and write fails as a backend error.
    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    /// Adds latency to every read and write, for exercising timeouts.
    pub fn set_delay(&self, delay: Duration) {
        self.delay_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// How many `get` calls actually reached the store - lets cache tests prove
    /// a hit did not touch the origin.
    pub fn get_count(&self) -> u64 {
        self.gets.load(Ordering::SeqCst)
    }

    /// Seeds an object directly, bypassing failure injection.
    pub fn insert(&self, key: &str, bytes: impl Into<Bytes>) {
        let etag = self.fresh_etag();
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_owned(), (bytes.into(), etag));
    }

    pub fn contains(&self, key: &str) -> bool {
        self.objects.lock().unwrap().contains_key(key)
    }

    fn fresh_etag(&self) -> String {
        format!("\"etag-{}\"", self.next_etag.fetch_add(1, Ordering::SeqCst))
    }

    async fn simulate(&self) -> Result<(), StoreError> {
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        if self.failing.load(Ordering::SeqCst) {
            return Err(StoreError::Backend("injected failure".to_owned()));
        }
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for InMemoryStore {
    async fn get(&self, key: &str) -> Result<Object, StoreError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.simulate().await?;
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(bytes, etag)| Object {
                bytes: bytes.clone(),
                etag: Some(etag.clone()),
            })
            .ok_or_else(|| StoreError::NotFound(key.to_owned()))
    }

    async fn put(
        &self,
        key: &str,
        body: Bytes,
        _content_type: &str,
        condition: PutCondition,
    ) -> Result<Option<String>, StoreError> {
        self.simulate().await?;
        let mut objects = self.objects.lock().unwrap();
        let current = objects.get(key).map(|(_, etag)| etag.as_str());
        let allowed = match &condition {
            PutCondition::None => true,
            PutCondition::IfMatch(expected) => current == Some(expected.as_str()),
            PutCondition::IfNoneMatch => current.is_none(),
        };
        if !allowed {
            return Err(StoreError::PreconditionFailed(key.to_owned()));
        }
        let etag = self.fresh_etag();
        objects.insert(key.to_owned(), (body, etag.clone()));
        Ok(Some(etag))
    }

    async fn put_file(&self, key: &str, path: &Path, content_type: &str) -> Result<(), StoreError> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| StoreError::Backend(format!("reading {}: {e}", path.display())))?;
        self.put(key, bytes.into(), content_type, PutCondition::None)
            .await
            .map(|_| ())
    }

    async fn head(&self, key: &str) -> Result<u64, StoreError> {
        self.simulate().await?;
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(bytes, _)| bytes.len() as u64)
            .ok_or_else(|| StoreError::NotFound(key.to_owned()))
    }

    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, StoreError> {
        Ok(format!("memory://{key}?ttl={}", ttl.as_secs()))
    }
}

#[cfg(feature = "s3")]
pub use s3::{S3Config, S3Store};

#[cfg(feature = "s3")]
mod s3 {
    use std::path::Path;
    use std::time::Duration;

    use async_trait::async_trait;
    use aws_credential_types::Credentials;
    use aws_sdk_s3::Client;
    use aws_sdk_s3::config::{BehaviorVersion, Region};
    use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
    use aws_sdk_s3::presigning::PresigningConfig;
    use aws_sdk_s3::primitives::ByteStream;
    use bytes::Bytes;

    use super::{Object, ObjectStore, PutCondition, StoreError};

    #[derive(Debug, Clone)]
    pub struct S3Config {
        /// Where this process talks to the S3 API, e.g. the in-cluster Service.
        pub endpoint: String,
        /// The host embedded in presigned URLs. Often NOT `endpoint`: a
        /// presigned URL is followed by the updater client, which cannot
        /// resolve an in-cluster Service name. The SigV4 signature covers the
        /// Host header, so the URL has to be signed for the host the client
        /// will actually dial - rewriting the host afterwards breaks it.
        pub presign_endpoint: String,
        pub bucket: String,
        pub region: String,
        pub access_key: String,
        pub secret_key: String,
    }

    pub struct S3Store {
        client: Client,
        presign_client: Client,
        bucket: String,
    }

    impl S3Store {
        pub fn new(config: S3Config) -> Self {
            let credentials = Credentials::new(
                config.access_key.clone(),
                config.secret_key.clone(),
                None,
                None,
                "tanukistore-static",
            );
            let build = |endpoint: &str| {
                let conf = aws_sdk_s3::Config::builder()
                    .behavior_version(BehaviorVersion::latest())
                    .region(Region::new(config.region.clone()))
                    .endpoint_url(endpoint)
                    .credentials_provider(credentials.clone())
                    // MinIO serves buckets by path, not by virtual-host DNS.
                    .force_path_style(true)
                    .build();
                Client::from_conf(conf)
            };
            S3Store {
                client: build(&config.endpoint),
                presign_client: build(&config.presign_endpoint),
                bucket: config.bucket,
            }
        }
    }

    fn classify<E, R>(key: &str, err: SdkError<E, R>) -> StoreError
    where
        E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
        R: std::fmt::Debug + HttpStatus,
    {
        let status = err.raw_response().map(HttpStatus::status_code);
        let code = err
            .as_service_error()
            .and_then(|e| e.code().map(str::to_owned));
        match (status, code.as_deref()) {
            (Some(404), _) | (_, Some("NoSuchKey" | "NotFound")) => {
                StoreError::NotFound(key.to_owned())
            }
            (Some(412), _) | (_, Some("PreconditionFailed")) => {
                StoreError::PreconditionFailed(key.to_owned())
            }
            _ => StoreError::Backend(format!(
                "{key}: {}",
                aws_sdk_s3::error::DisplayErrorContext(&err)
            )),
        }
    }

    trait HttpStatus {
        fn status_code(&self) -> u16;
    }

    impl HttpStatus for aws_sdk_s3::config::http::HttpResponse {
        fn status_code(&self) -> u16 {
            self.status().as_u16()
        }
    }

    #[async_trait]
    impl ObjectStore for S3Store {
        async fn get(&self, key: &str) -> Result<Object, StoreError> {
            let out = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| classify(key, e))?;
            let etag = out.e_tag().map(str::to_owned);
            let bytes = out
                .body
                .collect()
                .await
                .map_err(|e| StoreError::Backend(format!("{key}: reading body: {e}")))?
                .into_bytes();
            Ok(Object { bytes, etag })
        }

        async fn put(
            &self,
            key: &str,
            body: Bytes,
            content_type: &str,
            condition: PutCondition,
        ) -> Result<Option<String>, StoreError> {
            let mut request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .content_type(content_type)
                .body(ByteStream::from(body));
            request = match condition {
                PutCondition::None => request,
                PutCondition::IfMatch(etag) => request.if_match(etag),
                PutCondition::IfNoneMatch => request.if_none_match("*"),
            };
            let out = request.send().await.map_err(|e| classify(key, e))?;
            Ok(out.e_tag().map(str::to_owned))
        }

        async fn put_file(
            &self,
            key: &str,
            path: &Path,
            content_type: &str,
        ) -> Result<(), StoreError> {
            let body = ByteStream::from_path(path)
                .await
                .map_err(|e| StoreError::Backend(format!("opening {}: {e}", path.display())))?;
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .content_type(content_type)
                .body(body)
                .send()
                .await
                .map_err(|e| classify(key, e))?;
            Ok(())
        }

        async fn head(&self, key: &str) -> Result<u64, StoreError> {
            let out = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| classify(key, e))?;
            Ok(out.content_length().unwrap_or(0).max(0) as u64)
        }

        async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, StoreError> {
            let config = PresigningConfig::expires_in(ttl)
                .map_err(|e| StoreError::Backend(format!("presign config: {e}")))?;
            let request = self
                .presign_client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .presigned(config)
                .await
                .map_err(|e| StoreError::Backend(format!("presigning {key}: {e}")))?;
            Ok(request.uri().to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conditional_puts_enforce_optimistic_concurrency() {
        let store = InMemoryStore::new();
        let first = store
            .put(
                "k",
                Bytes::from_static(b"a"),
                "x",
                PutCondition::IfNoneMatch,
            )
            .await
            .unwrap()
            .unwrap();
        // A second create-only write loses.
        assert_eq!(
            store
                .put(
                    "k",
                    Bytes::from_static(b"b"),
                    "x",
                    PutCondition::IfNoneMatch
                )
                .await,
            Err(StoreError::PreconditionFailed("k".to_owned()))
        );
        // Writing against the current ETag wins and rotates it...
        let second = store
            .put(
                "k",
                Bytes::from_static(b"c"),
                "x",
                PutCondition::IfMatch(first.clone()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first, second);
        // ...so a writer still holding the old one is refused.
        assert_eq!(
            store
                .put(
                    "k",
                    Bytes::from_static(b"d"),
                    "x",
                    PutCondition::IfMatch(first)
                )
                .await,
            Err(StoreError::PreconditionFailed("k".to_owned()))
        );
        assert_eq!(
            store.get("k").await.unwrap().bytes,
            Bytes::from_static(b"c")
        );
    }

    #[test]
    fn only_timeouts_and_backend_errors_count_against_health() {
        assert!(StoreError::Timeout(Duration::from_secs(2)).is_backend_failure());
        assert!(StoreError::Backend("x".into()).is_backend_failure());
        assert!(!StoreError::NotFound("k".into()).is_backend_failure());
        assert!(!StoreError::PreconditionFailed("k".into()).is_backend_failure());
        assert!(!StoreError::BreakerOpen.is_backend_failure());
    }
}
