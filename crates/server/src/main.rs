use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tanukistore_core::breaker::{BreakerConfig, CircuitBreakerStore};
use tanukistore_core::cache::{CacheConfig, ManifestCache};
use tanukistore_core::store::{S3Config, S3Store};
use tanukistore_server::overload::BoundedListener;
use tanukistore_server::{AppState, DynStore, admin_router, public_router};
use tokio::net::TcpListener;

fn env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let endpoint = env("S3_ENDPOINT")?;
    let s3 = S3Config {
        presign_endpoint: env_or("S3_PRESIGN_ENDPOINT", &endpoint),
        endpoint,
        bucket: env("S3_BUCKET")?,
        region: env_or("S3_REGION", "us-east-1"),
        access_key: env("S3_ACCESS_KEY")?,
        secret_key: env("S3_SECRET_KEY")?,
    };
    let presign_ttl = Duration::from_secs(
        env_or("TANUKI_PRESIGN_TTL_SECS", "900")
            .parse()
            .context("TANUKI_PRESIGN_TTL_SECS must be an integer")?,
    );
    let max_in_flight: usize = env_or("TANUKI_MAX_IN_FLIGHT", "1024")
        .parse()
        .context("TANUKI_MAX_IN_FLIGHT must be an integer")?;
    let max_connections: usize = env_or("TANUKI_MAX_CONNECTIONS", "512")
        .parse()
        .context("TANUKI_MAX_CONNECTIONS must be an integer")?;
    let public_addr: SocketAddr = env_or("TANUKI_PUBLIC_ADDR", "0.0.0.0:8080").parse()?;
    let admin_addr: SocketAddr = env_or("TANUKI_ADMIN_ADDR", "0.0.0.0:9090").parse()?;

    let metrics = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("http_request_duration_seconds".to_owned()),
            &[
                0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
            ],
        )?
        .install_recorder()
        .context("installing the metrics recorder")?;
    // `install_recorder` does not start the exporter's upkeep task, and without
    // it every histogram sample is retained until the next /metrics scrape - an
    // unbounded buffer on any pod nobody scrapes.
    let upkeep = metrics.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            upkeep.run_upkeep();
        }
    });

    tracing::info!(bucket = %s3.bucket, endpoint = %s3.endpoint, "starting tanukistore-server");
    let store: DynStore = Arc::new(CircuitBreakerStore::new(
        S3Store::new(s3),
        BreakerConfig::default(),
    ));
    let state = Arc::new(AppState {
        cache: ManifestCache::new(store, CacheConfig::default()),
        presign_ttl,
        max_in_flight,
    });

    // Two listeners (spec 6.1), so /metrics is never reachable from the feed.
    let public = BoundedListener::new(
        TcpListener::bind(public_addr)
            .await
            .with_context(|| format!("binding {public_addr}"))?,
        max_connections,
    );
    let admin = TcpListener::bind(admin_addr)
        .await
        .with_context(|| format!("binding {admin_addr}"))?;
    tracing::info!(%public_addr, %admin_addr, max_in_flight, max_connections, "listening");

    let render = move || metrics.render();
    let public_server =
        axum::serve(public, public_router(state)).with_graceful_shutdown(shutdown_signal());
    let admin_server =
        axum::serve(admin, admin_router(render)).with_graceful_shutdown(shutdown_signal());
    tokio::try_join!(public_server, admin_server)?;
    tracing::info!("shut down cleanly");
    Ok(())
}

/// Resolves on SIGTERM (what k8s sends) or Ctrl-C, so in-flight requests finish
/// before the pod exits instead of being cut off mid-response.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
