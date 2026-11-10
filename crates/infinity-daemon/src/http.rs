//! Minimal HTTP/1.1 serving helper shared by the RAP callback bridge
//! and the MCP proxy (both bind loopback only — D6).

use std::convert::Infallible;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// Serve `handle` on `listener` forever (one task per connection).
/// `handle` receives the request and returns the response.
pub async fn serve_http1<F, Fut>(listener: TcpListener, handle: Arc<F>)
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Response<Full<Bytes>>> + Send + 'static,
{
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("accept error: {e}");
                continue;
            }
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let handle = handle.clone();
                async move { Ok::<_, Infallible>(handle(req).await) }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::warn!("connection error: {e}");
            }
        });
    }
}
