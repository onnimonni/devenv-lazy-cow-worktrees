//! CLI side of the control API: JSON over HTTP/1.1 on the daemon's unix socket.

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde::{Serialize, de::DeserializeOwned};
use tokio::net::UnixStream;

pub async fn request<T: DeserializeOwned>(
    method: Method,
    path: &str,
    body: Option<&impl Serialize>,
) -> Result<T> {
    let socket = crate::config::socket_path();
    let stream = UnixStream::connect(&socket).await.with_context(|| {
        format!(
            "localforest is not running ({}); start it with `localforest serve` (devenv up)",
            socket.display()
        )
    })?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let body = match body {
        Some(b) => Bytes::from(serde_json::to_vec(b)?),
        None => Bytes::new(),
    };
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localforest")
        .header("content-type", "application/json")
        .body(Full::new(body))?;
    let resp = sender.send_request(req).await?;
    let status = resp.status();
    let bytes = resp.into_body().collect().await?.to_bytes();
    if !status.is_success() {
        bail!("{}", String::from_utf8_lossy(&bytes).trim());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn get<T: DeserializeOwned>(path: &str) -> Result<T> {
    request(Method::GET, path, None::<&()>).await
}

pub async fn post<T: DeserializeOwned>(path: &str, body: &impl Serialize) -> Result<T> {
    request(Method::POST, path, Some(body)).await
}
