//! PostgreSQL front: lazy-cow-tree owns the PostgreSQL port; the real server only listens
//! on a private unix socket on the RAM disk.
//!
//! `DATABASE_URL=postgres://<checkout id>:<password>@127.0.0.1:55432/<db>`. Every
//! checkout has its own role (named after it, password derived from a per-machine
//! secret). The startup message's user picks the checkout: before the connection is
//! piped through, the database it asks for is created if missing (a copy-on-write
//! clone of the project's template), and a checkout may only open its own databases
//! (plus `postgres` and `template1` for tools like `mix ecto.create`). The real server
//! then authenticates the password (scram) over the piped connection. Users that are
//! no registered checkout's role are refused before reaching the server.

use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixStream},
};
use tracing::{debug, info};

const SSL_REQUEST: i32 = 80877103;
const GSSENC_REQUEST: i32 = 80877104;
const CANCEL_REQUEST: i32 = 80877102;

/// Called with (user, database) before connecting: Ok to go ahead (after creating the
/// database if needed), Err to refuse.
pub type Resolve =
    Arc<dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

pub async fn serve(port: u16, backend: PathBuf, resolve: Resolve) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding PostgreSQL port {port}"))?;
    info!("PostgreSQL on 127.0.0.1:{port} (user picks the checkout)");
    loop {
        let (stream, _) = listener.accept().await?;
        let (backend, resolve) = (backend.clone(), resolve.clone());
        tokio::spawn(async move {
            if let Err(e) = connection(stream, &backend, &resolve).await {
                debug!("postgres connection: {e:#}");
            }
        });
    }
}

async fn read_packet(s: &mut TcpStream) -> Result<Vec<u8>> {
    let len = s.read_i32().await?;
    if !(8..=10_000).contains(&len) {
        bail!("bad startup packet length {len}");
    }
    let mut body = vec![0; len as usize - 4];
    s.read_exact(&mut body).await?;
    Ok(body)
}

fn parse_params(body: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut parts = body
        .split(|b| *b == 0)
        .map(|p| String::from_utf8_lossy(p).into_owned());
    while let (Some(k), Some(v)) = (parts.next(), parts.next()) {
        if k.is_empty() {
            break;
        }
        out.push((k, v));
    }
    out
}

fn startup(code: i32, params: &[(String, String)]) -> Vec<u8> {
    let mut body = code.to_be_bytes().to_vec();
    for (k, v) in params {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut out = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    out.extend(body);
    out
}

/// A FATAL ErrorResponse.
fn error(code: &str, msg: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (t, v) in [(b'S', "FATAL"), (b'V', "FATAL"), (b'C', code), (b'M', msg)] {
        body.push(t);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut out = vec![b'E'];
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend(body);
    out
}

async fn connection(mut client: TcpStream, backend: &PathBuf, resolve: &Resolve) -> Result<()> {
    client.set_nodelay(true)?;
    let (code, params) = loop {
        let body = read_packet(&mut client).await?;
        let code = i32::from_be_bytes(body[..4].try_into()?);
        match code {
            // No TLS on loopback; clients with sslmode=prefer carry on in plain text.
            SSL_REQUEST | GSSENC_REQUEST => client.write_all(b"N").await?,
            CANCEL_REQUEST => {
                let mut b = UnixStream::connect(backend).await?;
                let mut raw = ((body.len() + 4) as i32).to_be_bytes().to_vec();
                raw.extend(body);
                b.write_all(&raw).await?;
                return Ok(());
            }
            _ => break (code, parse_params(&body[4..])),
        }
    };
    let get = |k: &str| {
        params
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.clone())
    };
    let user = get("user").unwrap_or_default();
    let database = get("database").unwrap_or_else(|| user.clone());
    if let Err(e) = resolve(user, database).await {
        client.write_all(&error("3D000", &format!("{e:#}"))).await?;
        return Ok(());
    }
    let mut server = UnixStream::connect(backend)
        .await
        .context("connecting to PostgreSQL")?;
    server.write_all(&startup(code, &params)).await?;
    tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_roundtrip() {
        let p = vec![
            ("user".to_string(), "demo-wt".to_string()),
            ("database".to_string(), "demo_dev_wt".to_string()),
        ];
        let raw = startup(196608, &p);
        assert_eq!(
            i32::from_be_bytes(raw[..4].try_into().unwrap()) as usize,
            raw.len()
        );
        assert_eq!(parse_params(&raw[8..]), p);
        assert_eq!(error("3D000", "x")[0], b'E');
    }
}
