//! Overload protection, added after load testing OOM-killed the server.
//!
//! Past its capacity (~12.7k req/s per core in the mixed feed test) the server
//! used to accept every connection and queue every request. Clients kept
//! opening connections, each holding ~28KB of buffers, and memory went from
//! 29MiB to 199MiB in 40s before the pod was OOM-killed. Two bounds fix that:
//!
//! - a process-wide cap on IN-FLIGHT requests, with excess answered at once
//!   by a 503 instead of queued. A fast "try again" is worth more to an
//!   updater than a slow success: Squirrel polls again later anyway, and a
//!   queue past capacity only makes every client slow.
//! - a cap on OPEN CONNECTIONS, enforced at accept. Beyond it, new clients
//!   wait in the kernel's accept backlog instead of in this process's memory.
//!
//! Sizing, from the sweep in loadtest/RESULTS-tanukistore-2026-09-17.md:
//!
//! - The CONNECTION cap is what bounds latency. With HTTP/1.1 each connection
//!   carries one request at a time, so past capacity the wait is roughly
//!   (open connections x service time): 2048 gave a 313ms p95, 512 gave
//!   60-90ms, 256 gave 22ms.
//! - It must leave room for IDLE keep-alive connections, which hold a permit
//!   while doing nothing. 256 starved an ordinary 4k req/s load for that
//!   reason; 512 did not.
//! - The IN-FLIGHT cap must sit ABOVE the connection cap. Under HTTP/1.1
//!   in-flight can never exceed open connections, so a lower in-flight cap
//!   only fires in bursts - and it did, shedding 1% of a 4k req/s load
//!   against a cold cache. It remains as the bound for HTTP/2, where one
//!   connection multiplexes many requests.
//! - Capping costs goodput under overload (~7.4k vs ~12.8k req/s per core,
//!   same 52% kernel share either way) in exchange for bounded latency and
//!   memory. Uncapped, the same overload reached a 943ms p95 and 192MiB.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::BoxError;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// What the load-shed layer answers with once the in-flight cap is reached.
pub async fn overloaded(_: BoxError) -> Response {
    metrics::counter!("requests_shed_total").increment(1);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::RETRY_AFTER, "1"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "overloaded",
    )
        .into_response()
}

/// A `TcpListener` that holds one permit per open connection.
///
/// `accept` does not return until a permit is free, and the permit travels
/// inside the connection's IO object, so it is released exactly when hyper
/// drops the connection - however that happens.
pub struct BoundedListener {
    inner: TcpListener,
    permits: Arc<Semaphore>,
}

impl BoundedListener {
    pub fn new(inner: TcpListener, max_connections: usize) -> Self {
        BoundedListener {
            inner,
            permits: Arc::new(Semaphore::new(max_connections)),
        }
    }
}

impl Listener for BoundedListener {
    type Io = PermittedStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("the connection semaphore is never closed");
        // axum's own impl retries accept errors (EMFILE and friends) and logs.
        let (stream, addr) = Listener::accept(&mut self.inner).await;
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%error, "could not set TCP_NODELAY");
        }
        (
            PermittedStream {
                stream,
                _permit: permit,
            },
            addr,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

pub struct PermittedStream {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for PermittedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for PermittedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn connections_beyond_the_cap_wait_until_one_closes() {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let mut listener = BoundedListener::new(tcp, 1);

        let _a = TcpStream::connect(addr).await.unwrap();
        let _b = TcpStream::connect(addr).await.unwrap();
        let (first, _) = listener.accept().await;

        // The second client is connected at the TCP level (kernel backlog) but
        // must not be accepted while the first holds the only permit.
        let pending = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
        assert!(pending.is_err(), "accepted past the connection cap");

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), listener.accept()).await;
        assert!(second.is_ok(), "closing a connection must free its permit");
    }
}
