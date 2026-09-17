//! Route behavior against `InMemoryStore`, seeded with manifests produced by
//! the real `derive_*` functions - the same bytes the publisher writes.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{TimeZone, Utc};
use tanukistore_core::cache::{CacheConfig, ManifestCache};
use tanukistore_core::derive::{derive_latest, derive_releases};
use tanukistore_core::keys::{Coordinate, config_key};
use tanukistore_core::model::{Arch, Asset, AssetKind, Index, Platform, Release, RolloutPct};
use tanukistore_core::store::InMemoryStore;
use tanukistore_server::{AppState, DynStore, public_router};
use tower::ServiceExt;

const BASE: &str = "https://updates.example.com";
const SHA1: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

fn release(version: &str, pct: u8, assets: Vec<Asset>) -> Release {
    Release {
        version: semver::Version::parse(version).unwrap(),
        notes: format!("Notes for {version}"),
        pub_date: Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap(),
        rollout_pct: RolloutPct::new(pct).unwrap(),
        assets,
    }
}

fn asset(kind: AssetKind, filename: &str) -> Asset {
    Asset {
        kind,
        filename: filename.to_owned(),
        sha1: SHA1.to_owned(),
        sha512: "cf83e135".to_owned(),
        size_bytes: 1234,
    }
}

fn seeded() -> Arc<InMemoryStore> {
    let store = Arc::new(InMemoryStore::new());
    store.insert(
        &config_key("myapp").unwrap(),
        r#"{"channels":["stable","beta"],"defaultChannel":"stable"}"#,
    );

    let mac = Coordinate::new("myapp", "stable", Platform::Darwin, Arch::Arm64).unwrap();
    let mac_index = Index {
        releases: vec![
            release(
                "1.5.0",
                100,
                vec![
                    asset(AssetKind::Zip, "MyApp-1.5.0-mac.zip"),
                    asset(AssetKind::Dmg, "MyApp-1.5.0.dmg"),
                ],
            ),
            // Staged at 0%: never eligible, so `latest` must skip it.
            release(
                "2.0.0",
                0,
                vec![asset(AssetKind::Zip, "MyApp-2.0.0-mac.zip")],
            ),
        ],
    };
    store.insert(&mac.index_key(), serde_json::to_vec(&mac_index).unwrap());
    store.insert(
        &mac.latest_key(),
        derive_latest(&mac_index, &mac, BASE).unwrap().unwrap(),
    );

    let win = Coordinate::new("myapp", "stable", Platform::Win32, Arch::X64).unwrap();
    let win_index = Index {
        releases: vec![
            release(
                "1.4.0",
                100,
                vec![asset(AssetKind::Nupkg, "MyApp-1.4.0-full.nupkg")],
            ),
            release(
                "1.5.0",
                100,
                vec![
                    asset(AssetKind::Nupkg, "MyApp-1.5.0-full.nupkg"),
                    asset(AssetKind::Exe, "MyAppSetup-1.5.0.exe"),
                ],
            ),
        ],
    };
    store.insert(&win.index_key(), serde_json::to_vec(&win_index).unwrap());
    store.insert(&win.releases_key(), derive_releases(&win_index).unwrap());
    store
}

fn app(store: Arc<InMemoryStore>) -> axum::Router {
    let store: DynStore = store;
    public_router(Arc::new(AppState {
        cache: ManifestCache::new(store, CacheConfig::default()),
        presign_ttl: Duration::from_secs(900),
        max_in_flight: 256,
    }))
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = router
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn location(headers: &axum::http::HeaderMap) -> &str {
    headers[header::LOCATION].to_str().unwrap()
}

#[tokio::test]
async fn darwin_offers_the_newest_full_release_to_an_older_client() {
    let router = app(seeded());
    let (status, headers, body) = get(&router, "/update/myapp/darwin/arm64/1.4.0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let manifest: serde_json::Value = serde_json::from_str(&body).unwrap();
    // 2.0.0 is staged at 0%, so 1.5.0 is the newest release everyone may have.
    assert_eq!(manifest["name"], "1.5.0");
}

#[tokio::test]
async fn darwin_answers_204_to_a_current_or_newer_client() {
    let router = app(seeded());
    for version in ["1.5.0", "v1.5.0", "1.6.0-dev"] {
        let (status, _, body) =
            get(&router, &format!("/update/myapp/darwin/arm64/{version}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{version}");
        assert!(body.is_empty());
    }
}

#[tokio::test]
async fn darwin_distinguishes_misconfiguration_from_nothing_published() {
    let router = app(seeded());
    // Spec 11: a typo'd feed must be loud...
    assert_eq!(
        get(&router, "/update/nosuchapp/darwin/arm64/1.0.0").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&router, "/update/myapp/darwin/arm64/1.0.0/nightly")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&router, "/update/myapp/darwin/sparc/1.0.0").await.0,
        StatusCode::NOT_FOUND
    );
    // ...while a real channel that has not shipped yet is simply "no update",
    // or every pre-first-release client would show the user an error.
    assert_eq!(
        get(&router, "/update/myapp/darwin/arm64/1.0.0/beta")
            .await
            .0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn darwin_rejects_an_unparseable_client_version() {
    let router = app(seeded());
    assert_eq!(
        get(&router, "/update/myapp/darwin/arm64/not-a-version")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn the_url_embedded_in_latest_json_resolves_to_a_presigned_asset() {
    // Spec 4.7 end to end: follow the manifest's own url back into the server.
    let router = app(seeded());
    let (_, _, body) = get(&router, "/update/myapp/darwin/arm64/1.0.0").await;
    let manifest: serde_json::Value = serde_json::from_str(&body).unwrap();
    let url = manifest["url"].as_str().unwrap();
    let path_and_query = url
        .strip_prefix(BASE)
        .expect("absolute url on the public base");

    let (status, headers, _) = get(&router, path_and_query).await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(
        location(&headers),
        "memory://myapp/stable/darwin/arm64/1.5.0/MyApp-1.5.0-mac.zip?ttl=900"
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn win32_serves_the_releases_feed_verbatim() {
    let router = app(seeded());
    for uri in [
        "/update/myapp/win32/x64/RELEASES",
        "/update/myapp/win32/x64/stable/RELEASES",
    ] {
        let (status, headers, body) = get(&router, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        assert_eq!(
            body,
            format!("{SHA1} MyApp-1.4.0-full.nupkg 1234\n{SHA1} MyApp-1.5.0-full.nupkg 1234\n")
        );
    }
    assert_eq!(
        get(&router, "/update/myapp/win32/x64/beta/RELEASES")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn win32_resolves_relative_nupkg_filenames_to_presigned_urls() {
    let router = app(seeded());
    let (status, headers, _) = get(&router, "/update/myapp/win32/x64/MyApp-1.4.0-full.nupkg").await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(
        location(&headers),
        "memory://myapp/stable/win32/x64/1.4.0/MyApp-1.4.0-full.nupkg?ttl=900"
    );
    let (status, _, _) = get(
        &router,
        "/update/myapp/win32/x64/stable/MyApp-1.5.0-full.nupkg",
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
}

#[tokio::test]
async fn win32_refuses_to_presign_anything_the_index_does_not_list() {
    let router = app(seeded());
    for uri in [
        "/update/myapp/win32/x64/secret.bin",
        // Listed in the index, but as an installer - not a Squirrel package.
        "/update/myapp/win32/x64/MyAppSetup-1.5.0.exe",
        "/update/myapp/win32/x64/..%2F..%2Fconfig.json",
    ] {
        assert_eq!(get(&router, uri).await.0, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn download_latest_honours_rollout_and_prefers_installers() {
    let router = app(seeded());
    let (status, headers, _) =
        get(&router, "/download/myapp/latest?platform=darwin&arch=arm64").await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(
        location(&headers),
        "memory://myapp/stable/darwin/arm64/1.5.0/MyApp-1.5.0.dmg?ttl=900",
        "2.0.0 is at 0% and must not be handed out; the dmg beats the zip"
    );
    let (_, headers, _) = get(&router, "/download/myapp/latest?platform=win32&arch=x64").await;
    assert_eq!(
        location(&headers),
        "memory://myapp/stable/win32/x64/1.5.0/MyAppSetup-1.5.0.exe?ttl=900"
    );
    assert_eq!(
        get(&router, "/download/myapp/latest?platform=linux&arch=x64")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn download_version_requires_a_listed_asset() {
    let router = app(seeded());
    let base = "/download/myapp/1.5.0?platform=win32&arch=x64";
    assert_eq!(
        get(&router, &format!("{base}&filename=MyApp-1.5.0-full.nupkg"))
            .await
            .0,
        StatusCode::FOUND
    );
    assert_eq!(
        get(&router, &format!("{base}&filename=MyApp-1.4.0-full.nupkg"))
            .await
            .0,
        StatusCode::NOT_FOUND,
        "the filename belongs to a different version"
    );
    assert_eq!(get(&router, base).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn notes_come_from_the_index() {
    let router = app(seeded());
    let (status, _, body) = get(&router, "/notes/myapp/1.4.0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Notes for 1.4.0");
    assert_eq!(
        get(&router, "/notes/myapp/9.9.9").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn an_unreachable_origin_with_nothing_cached_is_a_503_with_retry_after() {
    let store = seeded();
    store.set_failing(true);
    let router = app(store);
    let (status, headers, _) = get(&router, "/update/myapp/darwin/arm64/1.0.0").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::RETRY_AFTER], "5");
}

#[tokio::test]
async fn a_warm_cache_keeps_serving_through_an_origin_outage() {
    let store = seeded();
    let router = app(store.clone());
    assert_eq!(
        get(&router, "/update/myapp/darwin/arm64/1.0.0").await.0,
        StatusCode::OK
    );
    store.set_failing(true);
    assert_eq!(
        get(&router, "/update/myapp/darwin/arm64/1.0.0").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn requests_beyond_the_in_flight_cap_are_shed_immediately() {
    let store = seeded();
    // Cold cache plus a slow origin keeps the first request in flight.
    store.set_delay(Duration::from_millis(300));
    let dyn_store: DynStore = store;
    let router = public_router(Arc::new(AppState {
        cache: ManifestCache::new(dyn_store, CacheConfig::default()),
        presign_ttl: Duration::from_secs(900),
        max_in_flight: 1,
    }));

    let slow = {
        let router = router.clone();
        tokio::spawn(async move { get(&router, "/update/myapp/darwin/arm64/1.0.0").await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A DIFFERENT route: the cap must be process-wide, not per route.
    let started = std::time::Instant::now();
    let (status, headers, _) = get(&router, "/update/myapp/win32/x64/RELEASES").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::RETRY_AFTER], "1");
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "shedding must answer at once, not wait for a slot"
    );

    assert_eq!(slow.await.unwrap().0, StatusCode::OK);
    // With the slot free again, requests are served.
    assert_eq!(
        get(&router, "/update/myapp/win32/x64/RELEASES").await.0,
        StatusCode::OK
    );
}
