//! Redis: one port for everything, one real `redis-server` per checkout behind it.
//!
//! `REDIS_URL=redis://:<checkout id>@127.0.0.1:6380/0`. A client's first AUTH (or
//! HELLO ... AUTH) picks the checkout by its password; the connection is then piped
//! to that checkout's redis-server on a private unix socket, started on first use.
//! So every worktree has its own keys, pub/sub and FLUSHALL, with real Redis
//! semantics (Lua, streams, ...), and apps need nothing but the URL.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use bytes::{Buf, BytesMut};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixStream},
    process::Child,
    sync::Mutex,
};
use tracing::{debug, info, warn};

pub struct Redis {
    dir: PathBuf,
    /// Checkout ids (passwords) that may connect.
    known: RwLock<HashSet<String>>,
    procs: Mutex<HashMap<String, Child>>,
    /// A connection counts as its checkout's activity.
    activity: crate::history::Activity,
    /// `redis-server` to run (None: from PATH).
    server: Option<PathBuf>,
}

impl Redis {
    pub fn new(dir: PathBuf, activity: crate::history::Activity, server: Option<PathBuf>) -> Self {
        Self {
            dir,
            known: RwLock::default(),
            procs: Mutex::default(),
            activity,
            server,
        }
    }

    pub fn allow(&self, id: &str) {
        self.known.write().unwrap().insert(id.to_string());
    }

    /// Short file stem: sockets must fit in 104 bytes.
    fn stem(&self, id: &str) -> PathBuf {
        let h = Sha256::digest(id.as_bytes());
        self.dir.join(hex::encode(&h[..6]))
    }

    fn socket(&self, id: &str) -> PathBuf {
        self.stem(id).with_extension("sock")
    }

    pub fn running(&self, id: &str) -> bool {
        self.socket(id).exists()
    }

    /// The checkout's redis-server socket, starting the server if needed.
    async fn ensure(&self, id: &str) -> Result<PathBuf> {
        let socket = self.socket(id);
        let mut procs = self.procs.lock().await;
        if let Some(c) = procs.get_mut(id)
            && c.try_wait()?.is_none()
            && UnixStream::connect(&socket).await.is_ok()
        {
            return Ok(socket);
        }
        std::fs::create_dir_all(&self.dir)?;
        let pidfile = self.stem(id).with_extension("pid");
        // A redis-server left by a crashed daemon.
        if let Ok(pid) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_file(&socket);
        let server = match &self.server {
            Some(s) => s.clone(),
            None => crate::postgres::which("redis-server")
                .context("`redis-server` not in PATH; add pkgs.redis to devenv.nix packages")?,
        };
        let log = std::fs::File::create(self.stem(id).with_extension("log"))?;
        let child = tokio::process::Command::new(server)
            .args(["--port", "0", "--unixsocket"])
            .arg(&socket)
            .args([
                "--unixsocketperm",
                "700",
                "--save",
                "",
                "--appendonly",
                "no",
            ])
            .arg("--dir")
            .arg(&self.dir)
            .arg("--pidfile")
            .arg(&pidfile)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .context("starting redis-server")?;
        procs.insert(id.to_string(), child);
        for _ in 0..250 {
            if UnixStream::connect(&socket).await.is_ok() {
                info!("redis-server for {id} on {}", socket.display());
                return Ok(socket);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        bail!("redis-server for {id} did not start")
    }

    /// Stop and forget a checkout's redis-server (its data is gone).
    pub async fn remove(&self, id: &str) {
        self.known.write().unwrap().remove(id);
        if let Some(mut c) = self.procs.lock().await.remove(id) {
            let _ = c.kill().await;
        }
        for ext in ["sock", "pid", "log"] {
            let _ = std::fs::remove_file(self.stem(id).with_extension(ext));
        }
    }

    pub async fn stop_all(&self) {
        for (_, mut c) in self.procs.lock().await.drain() {
            let _ = c.kill().await;
        }
    }

    pub async fn serve(self: Arc<Self>, port: u16) -> Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("binding Redis port {port}"))?;
        info!("Redis on 127.0.0.1:{port} (password picks the checkout)");
        loop {
            let (stream, _) = listener.accept().await?;
            let r = self.clone();
            tokio::spawn(async move {
                if let Err(e) = r.connection(stream).await {
                    debug!("redis connection: {e:#}");
                }
            });
        }
    }

    async fn connection(&self, mut client: TcpStream) -> Result<()> {
        client.set_nodelay(true)?;
        let mut buf = BytesMut::with_capacity(4096);
        // Answer until the client authenticates.
        let (id, first) = loop {
            let args = loop {
                match parse(&mut buf) {
                    Ok(Some(a)) if a.is_empty() => continue,
                    Ok(Some(a)) => break a,
                    Ok(None) => {
                        if client.read_buf(&mut buf).await? == 0 {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        client
                            .write_all(format!("-ERR Protocol error: {e}\r\n").as_bytes())
                            .await?;
                        return Ok(());
                    }
                }
            };
            let cmd = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
            let (password, forward) = match cmd.as_str() {
                "AUTH" if args.len() == 2 || args.len() == 3 => {
                    (args[args.len() - 1].clone(), None)
                }
                "HELLO" => match args.iter().position(|a| a.eq_ignore_ascii_case(b"AUTH")) {
                    Some(p) if p + 2 < args.len() => {
                        // HELLO without the AUTH part: the backend has no password.
                        let mut rest = args[..p].to_vec();
                        rest.extend_from_slice(&args[p + 3..]);
                        (args[p + 2].clone(), Some(encode(&rest)))
                    }
                    _ => {
                        client
                            .write_all(b"-NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time\r\n")
                            .await?;
                        continue;
                    }
                },
                "QUIT" => {
                    client.write_all(b"+OK\r\n").await?;
                    return Ok(());
                }
                _ => {
                    client
                        .write_all(b"-NOAUTH Authentication required.\r\n")
                        .await?;
                    continue;
                }
            };
            let id = String::from_utf8_lossy(&password).into_owned();
            if !self.known.read().unwrap().contains(&id) {
                client
                    .write_all(
                        b"-WRONGPASS invalid username-password pair or user is disabled.\r\n",
                    )
                    .await?;
                continue;
            }
            self.activity.touch(&id);
            break (id, forward);
        };

        let socket = match self.ensure(&id).await {
            Ok(s) => s,
            Err(e) => {
                warn!("{e:#}");
                client
                    .write_all(format!("-ERR localforest: {e}\r\n").as_bytes())
                    .await?;
                return Ok(());
            }
        };
        let mut backend = UnixStream::connect(&socket).await?;
        match first {
            // HELLO: the backend answers it.
            Some(hello) => backend.write_all(&hello).await?,
            None => client.write_all(b"+OK\r\n").await?,
        }
        if !buf.is_empty() {
            backend.write_all(&buf).await?;
        }
        tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
        Ok(())
    }
}

fn encode(args: &[Vec<u8>]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// One RESP array of bulk strings, or an inline command. None: need more bytes.
fn parse(buf: &mut BytesMut) -> std::result::Result<Option<Vec<Vec<u8>>>, String> {
    fn line(buf: &[u8], from: usize) -> Option<(usize, usize)> {
        let pos = buf[from..].windows(2).position(|w| w == b"\r\n")?;
        Some((from + pos, from + pos + 2))
    }
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] != b'*' {
        let Some(pos) = buf.iter().position(|b| *b == b'\n') else {
            return Ok(None);
        };
        let l = buf.split_to(pos + 1);
        let s = String::from_utf8_lossy(&l).trim().to_string();
        return Ok(Some(
            s.split_whitespace()
                .map(|w| w.as_bytes().to_vec())
                .collect(),
        ));
    }
    let Some((end, mut pos)) = line(buf, 0) else {
        return Ok(None);
    };
    let count: i64 = std::str::from_utf8(&buf[1..end])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("invalid multibulk length")?;
    let mut args = Vec::with_capacity(count.clamp(0, 64) as usize);
    for _ in 0..count {
        if pos >= buf.len() {
            return Ok(None);
        }
        if buf[pos] != b'$' {
            return Err(format!("expected '$', got '{}'", buf[pos] as char));
        }
        let Some((end, next)) = line(buf, pos) else {
            return Ok(None);
        };
        let len: usize = std::str::from_utf8(&buf[pos + 1..end])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or("invalid bulk length")?;
        if buf.len() < next + len + 2 {
            return Ok(None);
        }
        args.push(buf[next..next + len].to_vec());
        pos = next + len + 2;
    }
    buf.advance(pos);
    Ok(Some(args))
}

pub fn dir() -> PathBuf {
    crate::config::home().join("redis")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resp_and_inline() {
        let mut b = BytesMut::from(&b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\nPING\r\n*1\r\n$4\r\nPI"[..]);
        assert_eq!(
            parse(&mut b).unwrap(),
            Some(vec![b"GET".to_vec(), b"k".to_vec()])
        );
        assert_eq!(parse(&mut b).unwrap(), Some(vec![b"PING".to_vec()]));
        assert_eq!(parse(&mut b).unwrap(), None);
        assert_eq!(
            encode(&[b"HELLO".to_vec(), b"3".to_vec()]),
            b"*2\r\n$5\r\nHELLO\r\n$1\r\n3\r\n"
        );
    }
}
