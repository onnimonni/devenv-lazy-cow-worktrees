//! HTTPS reverse proxy: https://<worktree>.<project>.localhost -> 127.0.0.1:<port>,
//! websockets included (LiveView, Vite HMR). Plain HTTP redirects to HTTPS.
//! Browsers resolve *.localhost to loopback on their own; no /etc/hosts entries.
//! Hosts that devenv projects register (devenv_proxy.rs) go through the same
//! listeners, over HTTPS and plain HTTP like devenv's own proxy.

use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{
    Request, Response, StatusCode,
    body::Incoming,
    header::{self, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use rustls::{
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::devenv_proxy::{self, DevenvRoutes};

type Body = BoxBody<Bytes, hyper::Error>;

struct Route {
    port: u16,
    /// Also serves its subdomains.
    subdomains: bool,
    /// Checkout id, whose activity a request touches.
    id: String,
}

/// host -> route.
#[derive(Clone, Default)]
pub struct Routes {
    map: Arc<RwLock<HashMap<String, Route>>>,
    activity: crate::history::Activity,
}

impl Routes {
    pub fn new(activity: crate::history::Activity) -> Self {
        Self {
            map: Default::default(),
            activity,
        }
    }

    pub fn set(&self, host: String, port: u16, subdomains: bool, id: String) {
        self.map.write().unwrap().insert(
            host,
            Route {
                port,
                subdomains,
                id,
            },
        );
    }

    pub fn remove(&self, host: &str) {
        self.map.write().unwrap().remove(host);
    }

    /// Whether a request for `host` would reach a checkout, without counting as
    /// activity.
    pub fn serves(&self, host: &str) -> bool {
        let routes = self.map.read().unwrap();
        if routes.contains_key(host) {
            return true;
        }
        let mut h = host;
        while let Some((_, parent)) = h.split_once('.') {
            if let Some(r) = routes.get(parent) {
                return r.subdomains;
            }
            h = parent;
        }
        false
    }

    /// Exact host, else the closest parent that serves subdomains (worktree hosts:
    /// debug.wt.web.app.localhost -> wt.web.app.localhost). Primary hosts don't, so a
    /// removed worktree's host never silently reaches the primary checkout. Counts as
    /// activity of the checkout.
    pub fn lookup(&self, host: &str) -> Option<u16> {
        let routes = self.map.read().unwrap();
        let route = routes.get(host).or_else(|| {
            let mut h = host;
            while let Some((_, parent)) = h.split_once('.') {
                if let Some(r) = routes.get(parent) {
                    return r.subdomains.then_some(r);
                }
                h = parent;
            }
            None
        })?;
        self.activity.touch(&route.id);
        Some(route.port)
    }
}

/// Renders the dashboard served at https://localforest.localhost.
pub type Dashboard = Arc<dyn Fn() -> String + Send + Sync>;

/// Called with the host when nothing listens on its port: starts the worktree's
/// server and resolves once it listens, or with why it couldn't (a failed
/// migration, say), shown on the 502 page.
pub type Ensure = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync,
>;

/// A request to a host without a route (method, host, path, Origin header).
pub struct Unrouted {
    pub method: hyper::Method,
    pub host: String,
    pub path: String,
    pub origin: Option<String>,
}

/// A page the daemon answers an unrouted request with.
pub struct Page {
    pub status: StatusCode,
    pub html: String,
    /// Redirect (303) instead.
    pub location: Option<String>,
}

/// Answers requests to hosts without a route (removed worktrees); None: plain 404.
pub type Fallback = Arc<
    dyn Fn(Unrouted) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Page>> + Send>>
        + Send
        + Sync,
>;

struct Shared {
    routes: Routes,
    devenv: DevenvRoutes,
    client: Client<HttpConnector, Incoming>,
    dashboard: Dashboard,
    ensure: Ensure,
    fallback: Fallback,
}

fn full(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Full::new(body.into()).map_err(|n| match n {}).boxed())
        .unwrap()
}

/// Bind `port`; below 1024 on all interfaces (unprivileged on macOS), else loopback.
async fn bind(port: u16) -> Result<TcpListener> {
    let addr: SocketAddr = if port < 1024 {
        ([0, 0, 0, 0], port).into()
    } else {
        ([127, 0, 0, 1], port).into()
    };
    Ok(TcpListener::bind(addr).await?)
}

/// A devenv route's own certificate (its project's mkcert CA), else a leaf from
/// localforest's CA.
#[derive(Debug)]
struct Certificates {
    ca: Arc<crate::tls::Ca>,
    devenv: DevenvRoutes,
}

impl ResolvesServerCert for Certificates {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(cert) = hello.server_name().and_then(|h| self.devenv.certificate(h)) {
            return Some(cert);
        }
        self.ca.resolve(hello)
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    https_port: u16,
    http_port: u16,
    routes: Routes,
    devenv: DevenvRoutes,
    ca: Arc<crate::tls::Ca>,
    dashboard: Dashboard,
    ensure: Ensure,
    fallback: Fallback,
) -> Result<()> {
    let certificates = Arc::new(Certificates {
        ca,
        devenv: devenv.clone(),
    });
    let tls = TlsAcceptor::from(Arc::new(crate::tls::server_config(certificates)));
    let shared = Arc::new(Shared {
        routes,
        devenv,
        client: Client::builder(TokioExecutor::new()).build_http(),
        dashboard,
        ensure,
        fallback,
    });
    let https = bind(https_port).await.map_err(|e| {
        anyhow::anyhow!("binding HTTPS port {https_port}: {e} (pick another with --https-port)")
    })?;
    info!("HTTPS proxy on port {https_port}");
    if http_port != 0 {
        match bind(http_port).await {
            Ok(l) => {
                tokio::spawn(http_loop(l, https_port, shared.clone()));
            }
            Err(e) => warn!("HTTP redirect on port {http_port} disabled: {e}"),
        }
    }
    loop {
        let (stream, peer) = https.accept().await?;
        if !peer.ip().is_loopback() {
            debug!("refusing {peer}");
            continue;
        }
        let tls = tls.clone();
        let shared = shared.clone();
        tokio::spawn(async move {
            let stream = match tls.accept(stream).await {
                Ok(s) => s,
                Err(e) => return debug!("TLS handshake with {peer}: {e}"),
            };
            let svc = service_fn(move |req| handle(shared.clone(), peer, req, "https"));
            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades()
                .await
            {
                debug!("connection from {peer}: {e}");
            }
        });
    }
}

/// Plain HTTP: devenv's health probe and devenv routes are answered here, like
/// devenv's own proxy; everything else redirects to HTTPS.
async fn http_loop(listener: TcpListener, https_port: u16, shared: Arc<Shared>) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let shared = shared.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let shared = shared.clone();
                async move {
                    let host = request_host(&req);
                    if host == devenv_proxy::HEALTH_HOSTNAME {
                        return Ok(full(StatusCode::NO_CONTENT, "text/plain", Bytes::new()));
                    }
                    if shared.devenv.resolve(&host).is_some() {
                        return handle(shared, peer, req, "http").await;
                    }
                    let port = if https_port == 443 {
                        String::new()
                    } else {
                        format!(":{https_port}")
                    };
                    let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
                    let host = if host.is_empty() { "localhost" } else { &host };
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(StatusCode::PERMANENT_REDIRECT)
                            .header(header::LOCATION, format!("https://{host}{port}{path}"))
                            .body(Full::new(Bytes::new()).map_err(|n| match n {}).boxed())
                            .unwrap(),
                    )
                }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades()
                .await;
        });
    }
}

/// Lowercased Host header (else the URI's host) without its port.
fn request_host(req: &Request<Incoming>) -> String {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| req.uri().host())
        .unwrap_or_default();
    strip_port(host).to_ascii_lowercase()
}

/// Host header without its port; an IPv6 literal keeps its brackets (`[::1]:8443` ->
/// `[::1]`).
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map_or(host, |end| &host[..=end]);
    }
    host.split(':').next().unwrap_or(host)
}

fn is_upgrade(req: &Request<Incoming>) -> bool {
    req.headers()
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
}

async fn handle(
    shared: Arc<Shared>,
    peer: SocketAddr,
    req: Request<Incoming>,
    scheme: &'static str,
) -> Result<Response<Body>, Infallible> {
    let host = request_host(&req);
    if host == "localforest.localhost" || host == "localhost" {
        return Ok(full(
            StatusCode::OK,
            "text/html; charset=utf-8",
            (shared.dashboard)(),
        ));
    }
    let Some(port) = shared.routes.lookup(&host) else {
        if let Some(upstream) = shared.devenv.resolve(&host) {
            let upstream = devenv_proxy::reachable(upstream).await;
            return Ok(forward(&shared, req, peer, &host, upstream, scheme)
                .await
                .unwrap_or_else(|e| {
                    full(
                        StatusCode::BAD_GATEWAY,
                        "text/plain; charset=utf-8",
                        format!("localforest: nothing answers for {host} on {upstream} ({e}); devenv registered it\n"),
                    )
                }));
        }
        let unrouted = Unrouted {
            method: req.method().clone(),
            host: host.clone(),
            path: req.uri().path().to_string(),
            origin: req
                .headers()
                .get(header::ORIGIN)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        };
        if let Some(page) = (shared.fallback)(unrouted).await {
            let mut resp = full(page.status, "text/html; charset=utf-8", page.html);
            if let Some(loc) = page.location
                && let Ok(v) = HeaderValue::from_str(&loc)
            {
                *resp.status_mut() = StatusCode::SEE_OTHER;
                resp.headers_mut().insert(header::LOCATION, v);
            }
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return Ok(resp);
        }
        return Ok(full(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            format!("localforest: no worktree serves {host}; see https://localforest.localhost\n"),
        ));
    };

    if tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err()
        && let Some(e) = (shared.ensure)(host.clone()).await
    {
        return Ok(full(
            StatusCode::BAD_GATEWAY,
            "text/plain; charset=utf-8",
            format!(
                "localforest: could not start {host}: {e}\nSee `localforest status` and the logs in ~/.local/state/localforest/logs.\n"
            ),
        ));
    }
    let upstream = SocketAddr::from(([127, 0, 0, 1], port));
    Ok(forward(&shared, req, peer, &host, upstream, scheme)
        .await
        .unwrap_or_else(|e| {
            full(
                StatusCode::BAD_GATEWAY,
                "text/plain; charset=utf-8",
                format!(
                    "localforest: nothing answers for {host} on 127.0.0.1:{port} ({e}).\nSet localforest.server (devenv.nix) to start it on demand, or run it with PORT={port} (see `localforest env`); logs: `localforest server log <name>`.\n"
                ),
            )
        }))
}

/// Proxy `req` to `upstream`, websockets included.
async fn forward(
    shared: &Shared,
    mut req: Request<Incoming>,
    peer: SocketAddr,
    host: &str,
    upstream: SocketAddr,
    scheme: &'static str,
) -> Result<Response<Body>, hyper_util::client::legacy::Error> {
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
    let Ok(uri) = format!("http://{upstream}{path}").parse() else {
        return Ok(full(StatusCode::BAD_REQUEST, "text/plain", "bad uri\n"));
    };
    *req.uri_mut() = uri;
    let h = req.headers_mut();
    h.insert("x-forwarded-proto", HeaderValue::from_static(scheme));
    if let Ok(v) = HeaderValue::from_str(host) {
        h.insert("x-forwarded-host", v);
    }
    if let Ok(v) = HeaderValue::from_str(&peer.ip().to_string()) {
        h.insert("x-forwarded-for", v);
    }

    let upgrade = is_upgrade(&req);
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
    let mut resp = shared.client.request(req).await?;
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(client_upgrade) = client_upgrade
    {
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            match tokio::try_join!(client_upgrade, upstream_upgrade) {
                Ok((c, u)) => {
                    let _ =
                        tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(u))
                            .await;
                }
                Err(e) => debug!("upgrade failed: {e}"),
            }
        });
    }
    Ok(resp.map(|b| b.boxed()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_port_from_host() {
        assert_eq!(strip_port("[::1]:8443"), "[::1]");
        assert_eq!(strip_port("[::1]"), "[::1]");
        assert_eq!(strip_port("host:443"), "host");
        assert_eq!(strip_port("host"), "host");
        assert_eq!(strip_port(""), "");
    }

    #[test]
    fn lookup_falls_back_to_parent() {
        let r = Routes::default();
        r.set("app.localhost".into(), 4000, false, "app".into());
        r.set("api.app.localhost".into(), 4001, false, "app".into());
        r.set("wt.api.app.localhost".into(), 20010, true, "app-wt".into());
        assert_eq!(r.lookup("wt.api.app.localhost"), Some(20010));
        assert_eq!(r.lookup("debug.wt.api.app.localhost"), Some(20010));
        assert!(r.activity.get("app-wt").is_some());
        assert!(r.activity.get("app").is_none());
        // A removed worktree's host doesn't reach the primary's api.
        assert_eq!(r.lookup("gone.api.app.localhost"), None);
        assert_eq!(r.lookup("other.app.localhost"), None);
        assert_eq!(r.lookup("nope.localhost"), None);
    }
}
