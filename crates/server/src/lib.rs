//! HTTP surface of tanukistore (spec 6).
//!
//! The server never derives or writes anything. It serves bytes the publisher
//! precomputed, from the manifest cache, and turns download requests into
//! redirects to freshly presigned object URLs - so no asset byte ever passes
//! through this process.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{MatchedPath, Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use semver::Version;
use serde::Deserialize;
use tanukistore_core::cache::{CacheError, Lookup, ManifestCache, Parsed};
use tanukistore_core::keys::{Coordinate, KeyKind, config_key};
use tanukistore_core::model::{Arch, AssetKind, Index, Platform, Release};
use tanukistore_core::resolve::resolve_eligible;
use tanukistore_core::store::ObjectStore;
use tower::ServiceBuilder;
use tower::limit::GlobalConcurrencyLimitLayer;

pub mod overload;

pub type DynStore = Arc<dyn ObjectStore>;

pub struct AppState {
    pub cache: ManifestCache<DynStore>,
    /// Spec 6.1: presigning happens only when a download starts, so the URL
    /// merely has to outlive connection setup and a client retry.
    pub presign_ttl: Duration,
    /// Requests allowed in flight across ALL routes before new ones are shed
    /// with a 503. See `overload`.
    pub max_in_flight: usize,
}

pub fn public_router(state: Arc<AppState>) -> Router {
    let max_in_flight = state.max_in_flight;
    Router::new()
        // axum has no optional path segments, so each `[/:channel]` variant is
        // registered twice (spec 6).
        .route("/update/{app}/darwin/{arch}/{version}", get(darwin_default))
        .route(
            "/update/{app}/darwin/{arch}/{version}/{channel}",
            get(darwin_channel),
        )
        // Static `RELEASES` outranks the capture at the same depth. The capture
        // is named `{name}` in all three routes because the router rejects
        // different parameter names at one position - it is a filename in the
        // first route and a channel in the other two.
        .route("/update/{app}/win32/{arch}/RELEASES", get(releases_default))
        .route("/update/{app}/win32/{arch}/{name}", get(nupkg_default))
        .route(
            "/update/{app}/win32/{arch}/{name}/RELEASES",
            get(releases_channel),
        )
        .route(
            "/update/{app}/win32/{arch}/{name}/{filename}",
            get(nupkg_channel),
        )
        .route("/download/{app}/latest", get(download_latest))
        .route("/download/{app}/{version}", get(download_version))
        .route("/notes/{app}/{version}", get(notes))
        // GLOBAL, not per-route: `Router::layer` instantiates a layer for each
        // route, and a plain ConcurrencyLimitLayer would then give every route
        // its own budget - a cap nine times larger than the number configured.
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(overload::overloaded))
                .load_shed()
                .layer(GlobalConcurrencyLimitLayer::new(max_in_flight)),
        )
        // Outermost, so shed requests are counted too.
        .layer(middleware::from_fn(record_request))
        .with_state(state)
}

pub fn admin_router(render_metrics: impl Fn() -> String + Clone + Send + Sync + 'static) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // Readiness deliberately ignores the breaker (spec 14): with it open the
        // service still answers from cache, and failing readiness would pull
        // working pods out of rotation.
        .route("/readyz", get(|| async { "ok" }))
        .route(
            "/metrics",
            get(move || {
                let render = render_metrics.clone();
                async move { render() }
            }),
        )
}

async fn record_request(request: Request, next: Next) -> Response {
    // The matched route TEMPLATE, never the raw path: raw paths carry app names,
    // versions and filenames, which would make the label unbounded.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16().to_string();
    metrics::histogram!("http_request_duration_seconds", "route" => route.clone())
        .record(started.elapsed().as_secs_f64());
    metrics::counter!("http_requests_total", "route" => route, "status" => status).increment(1);
    response
}

#[derive(Debug)]
pub enum ApiError {
    NotFound(&'static str),
    BadRequest(String),
    Unavailable,
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound(what) => (StatusCode::NOT_FOUND, what).into_response(),
            ApiError::BadRequest(why) => (StatusCode::BAD_REQUEST, why).into_response(),
            ApiError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "5")],
                "storage unavailable",
            )
                .into_response(),
            ApiError::Internal => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}

impl From<CacheError> for ApiError {
    fn from(error: CacheError) -> Self {
        match error {
            CacheError::Unavailable(_) => ApiError::Unavailable,
            CacheError::Corrupt { key, reason } => {
                tracing::error!(key, reason, "corrupt object in bucket");
                ApiError::Internal
            }
        }
    }
}

// --- shared resolution --------------------------------------------------------

async fn resolve_channel(
    state: &AppState,
    app: &str,
    requested: Option<&str>,
) -> Result<String, ApiError> {
    // A name that cannot be a key cannot be a configured app either.
    let key = config_key(app).map_err(|_| ApiError::NotFound("unknown app"))?;
    let lookup = state.cache.get(&key, KeyKind::Config).await?;
    let Some((_, Parsed::Config(config))) = &lookup.object.body else {
        // Spec 11: an absent config.json is misconfiguration, so it is loud.
        return Err(ApiError::NotFound("unknown app"));
    };
    config
        .resolve_channel(requested)
        .map(str::to_owned)
        .ok_or(ApiError::NotFound("unknown channel"))
}

async fn coordinate(
    state: &AppState,
    app: &str,
    requested_channel: Option<&str>,
    platform: Platform,
    arch: &str,
) -> Result<Coordinate, ApiError> {
    let arch: Arch = arch
        .parse()
        .map_err(|_| ApiError::NotFound("unknown arch"))?;
    let channel = resolve_channel(state, app, requested_channel).await?;
    Coordinate::new(app, channel, platform, arch).map_err(|_| ApiError::NotFound("unknown app"))
}

fn index_of(lookup: &Lookup) -> Option<&Index> {
    match &lookup.object.body {
        Some((_, Parsed::Index(index))) => Some(index),
        _ => None,
    }
}

async fn presigned_redirect(state: &AppState, key: &str) -> Result<Response, ApiError> {
    let url = state
        .cache
        .store()
        .presign_get(key, state.presign_ttl)
        .await
        .map_err(|error| {
            tracing::error!(key, %error, "presign failed");
            ApiError::Unavailable
        })?;
    let location = HeaderValue::from_str(&url).map_err(|_| ApiError::Internal)?;
    Ok((
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            // A presigned URL expires. Nothing between here and the client may
            // cache this redirect and replay a dead URL later.
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
    )
        .into_response())
}

fn parse_version(raw: &str) -> Result<Version, ApiError> {
    // Tags are routinely written `v1.2.3`; semver itself rejects the prefix.
    Version::parse(raw.strip_prefix('v').unwrap_or(raw))
        .map_err(|e| ApiError::BadRequest(format!("invalid version {raw:?}: {e}")))
}

// --- Squirrel.Mac -------------------------------------------------------------

async fn darwin_default(
    State(state): State<Arc<AppState>>,
    Path((app, arch, version)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    darwin(&state, &app, &arch, &version, None).await
}

async fn darwin_channel(
    State(state): State<Arc<AppState>>,
    Path((app, arch, version, channel)): Path<(String, String, String, String)>,
) -> Result<Response, ApiError> {
    darwin(&state, &app, &arch, &version, Some(&channel)).await
}

async fn darwin(
    state: &AppState,
    app: &str,
    arch: &str,
    version: &str,
    channel: Option<&str>,
) -> Result<Response, ApiError> {
    let client = parse_version(version)?;
    let coord = coordinate(state, app, channel, Platform::Darwin, arch).await?;
    let lookup = state
        .cache
        .get(&coord.latest_key(), KeyKind::Latest)
        .await?;

    // latest.json only ever names a release at 100% (derive.rs), so there is
    // no per-client rollout to evaluate here. See derive_latest's UNRESOLVED
    // note for why staged darwin releases are not yet reachable.
    let offer = match &lookup.object.body {
        Some((bytes, Parsed::Latest(latest))) if *latest > client => Some(bytes.clone()),
        // Nothing published yet, or the client is current (or ahead, as a dev
        // build is): 204, which Squirrel.Mac reads as "no update" (spec 11).
        _ => None,
    };
    count_check(&coord, offer.is_some());
    Ok(match offer {
        Some(bytes) => json(bytes),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

fn count_check(coord: &Coordinate, update_available: bool) {
    metrics::counter!(
        "update_checks_total",
        "app" => coord.app().to_owned(),
        "channel" => coord.channel().to_owned(),
        "platform" => coord.platform().to_string(),
        "arch" => coord.arch().to_string(),
        "update_available" => update_available.to_string(),
    )
    .increment(1);
}

fn json(bytes: Bytes) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
}

// --- Squirrel.Windows ---------------------------------------------------------

async fn releases_default(
    State(state): State<Arc<AppState>>,
    Path((app, arch)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    releases(&state, &app, &arch, None).await
}

async fn releases_channel(
    State(state): State<Arc<AppState>>,
    Path((app, arch, channel)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    releases(&state, &app, &arch, Some(&channel)).await
}

async fn releases(
    state: &AppState,
    app: &str,
    arch: &str,
    channel: Option<&str>,
) -> Result<Response, ApiError> {
    let coord = coordinate(state, app, channel, Platform::Win32, arch).await?;
    let lookup = state
        .cache
        .get(&coord.releases_key(), KeyKind::Releases)
        .await?;
    let Some((bytes, _)) = &lookup.object.body else {
        count_check(&coord, false);
        // Squirrel.Windows handles a missing feed; darwin's 204 has no analogue.
        return Err(ApiError::NotFound("nothing published"));
    };
    count_check(&coord, true);
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes.clone(),
    )
        .into_response())
}

async fn nupkg_default(
    State(state): State<Arc<AppState>>,
    Path((app, arch, filename)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    nupkg(&state, &app, &arch, None, &filename).await
}

async fn nupkg_channel(
    State(state): State<Arc<AppState>>,
    Path((app, arch, channel, filename)): Path<(String, String, String, String)>,
) -> Result<Response, ApiError> {
    nupkg(&state, &app, &arch, Some(&channel), &filename).await
}

/// Resolves the relative filenames RELEASES lists (spec 4.7).
async fn nupkg(
    state: &AppState,
    app: &str,
    arch: &str,
    channel: Option<&str>,
    filename: &str,
) -> Result<Response, ApiError> {
    let coord = coordinate(state, app, channel, Platform::Win32, arch).await?;
    let lookup = state.cache.get(&coord.index_key(), KeyKind::Index).await?;
    // Only presign a key the index actually names. Building the key from the
    // request alone would let a caller mint signed URLs for arbitrary objects.
    let release = index_of(&lookup)
        .and_then(|index| {
            index.releases.iter().find(|r| {
                r.assets
                    .iter()
                    .any(|a| a.kind == AssetKind::Nupkg && a.filename == filename)
            })
        })
        .ok_or(ApiError::NotFound("unknown package"))?;
    presigned_redirect(state, &coord.asset_key(&release.version, filename)).await
}

// --- downloads ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    platform: String,
    arch: String,
    channel: Option<String>,
    filename: Option<String>,
    cid: Option<String>,
    uid: Option<String>,
}

fn query_platform(raw: &str) -> Result<Platform, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::BadRequest(format!("unknown platform {raw:?}")))
}

async fn download_latest(
    State(state): State<Arc<AppState>>,
    Path(app): Path<String>,
    Query(q): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    let platform = query_platform(&q.platform)?;
    let coord = coordinate(&state, &app, q.channel.as_deref(), platform, &q.arch).await?;
    let lookup = state.cache.get(&coord.index_key(), KeyKind::Index).await?;
    let index = index_of(&lookup).ok_or(ApiError::NotFound("nothing published"))?;
    // Rollout applies here: `latest` is a moving target, so a staged release
    // must only be handed to clients whose bucket admits it. With no id the
    // client is ineligible for anything partial (spec 11, fail closed).
    let id = q.cid.as_deref().or(q.uid.as_deref());
    let release = resolve_eligible(index, coord.app(), coord.channel(), id)
        .ok_or(ApiError::NotFound("nothing published"))?;
    let asset = human_asset(release, platform).ok_or(ApiError::NotFound("no installable asset"))?;
    presigned_redirect(&state, &coord.asset_key(&release.version, &asset)).await
}

/// The file a person downloading by hand wants: the installer when the
/// release has one, the updater payload otherwise.
fn human_asset(release: &Release, platform: Platform) -> Option<String> {
    let preference: &[AssetKind] = match platform {
        Platform::Darwin => &[AssetKind::Dmg, AssetKind::Zip],
        Platform::Win32 => &[AssetKind::Exe, AssetKind::Nupkg],
    };
    preference.iter().find_map(|kind| {
        release
            .assets
            .iter()
            .find(|a| a.kind == *kind)
            .map(|a| a.filename.clone())
    })
}

async fn download_version(
    State(state): State<Arc<AppState>>,
    Path((app, version)): Path<(String, String)>,
    Query(q): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    let platform = query_platform(&q.platform)?;
    let version = parse_version(&version)?;
    let filename = q
        .filename
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("filename is required".to_owned()))?;
    let coord = coordinate(&state, &app, q.channel.as_deref(), platform, &q.arch).await?;
    let lookup = state.cache.get(&coord.index_key(), KeyKind::Index).await?;
    // A pinned version is a deliberate request - this is also the URL latest.json
    // embeds - so rollout is not re-applied. Existence in the index still is.
    let listed = index_of(&lookup).is_some_and(|index| {
        index
            .releases
            .iter()
            .any(|r| r.version == version && r.assets.iter().any(|a| a.filename == filename))
    });
    if !listed {
        return Err(ApiError::NotFound("unknown asset"));
    }
    presigned_redirect(&state, &coord.asset_key(&version, filename)).await
}

// --- notes --------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NotesQuery {
    channel: Option<String>,
}

async fn notes(
    State(state): State<Arc<AppState>>,
    Path((app, version)): Path<(String, String)>,
    Query(q): Query<NotesQuery>,
) -> Result<Response, ApiError> {
    let version = parse_version(&version)?;
    let channel = resolve_channel(&state, &app, q.channel.as_deref()).await?;
    // Notes are per release, but indexes are per platform/arch and the route
    // names neither, so take the first index that has the version.
    for (platform, arch) in [
        (Platform::Darwin, Arch::Arm64),
        (Platform::Darwin, Arch::X64),
        (Platform::Win32, Arch::X64),
        (Platform::Win32, Arch::Arm64),
    ] {
        let coord = Coordinate::new(&app, &channel, platform, arch)
            .map_err(|_| ApiError::NotFound("unknown app"))?;
        let lookup = state.cache.get(&coord.index_key(), KeyKind::Index).await?;
        if let Some(release) =
            index_of(&lookup).and_then(|index| index.releases.iter().find(|r| r.version == version))
        {
            return Ok((
                [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                release.notes.clone(),
            )
                .into_response());
        }
    }
    Err(ApiError::NotFound("unknown version"))
}
