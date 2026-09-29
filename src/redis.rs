//! Redis: one port for everything, one real `redis-server` per checkout behind it.
//!
//! `REDIS_URL=redis://:<checkout id>@127.0.0.1:6380/0`. A client's first AUTH (or
//! HELLO ... AUTH) picks the checkout by its password; the connection is then piped
//! to that checkout's redis-server on a private unix socket, started on first use.
//! So every worktree has its own keys, pub/sub and FLUSHALL, with real Redis
//! semantics (Lua, streams, ...), and apps need nothing but the URL. A project with
//! `LOCALFOREST_REDIS_INSTANCE=shared` maps all its checkouts to one redis-server.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
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
    /// Checkout ids (passwords) that may connect -> the redis-server they reach (the
    /// checkout's own, or its project's shared one).
    known: RwLock<HashMap<String, String>>,
    /// By redis-server key.
    procs: Mutex<HashMap<String, Child>>,
    /// Open client connections per redis-server key, and when the last one ended.
    conns: std::sync::Mutex<HashMap<String, (usize, Instant)>>,
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
            conns: Default::default(),
            activity,
            server,
        }
    }

    /// Let checkout `id` connect, to redis-server `backend` (`id` for its own).
    pub fn allow(&self, id: &str, backend: &str) {
        self.known
            .write()
            .unwrap()
            .insert(id.to_string(), backend.to_string());
    }

    /// Start redis-server `backend` now (`start = "up"`).
    pub async fn start(&self, backend: &str) -> Result<()> {
        self.ensure(backend).await.map(|_| ())
    }

    /// Stop redis-servers without connections for `timeout` (started again on the
    /// next one). Data isn't saved, as ever.
    pub async fn stop_idle(&self, timeout: Duration) {
        let mut procs = self.procs.lock().await;
        let idle: Vec<String> = {
            let conns = self.conns.lock().unwrap();
            procs
                .keys()
                .filter(|k| {
                    conns
                        .get(*k)
                        .is_none_or(|(open, last)| *open == 0 && last.elapsed() >= timeout)
                })
                .cloned()
                .collect()
        };
        for key in idle {
            // Never connected yet: counts from now.
            if !self.conns.lock().unwrap().contains_key(&key) {
                self.conns
                    .lock()
                    .unwrap()
                    .insert(key.clone(), (0, Instant::now()));
                continue;
            }
            if let Some(mut c) = procs.remove(&key) {
                info!(
                    "redis-server {key}: idle for {} s; stopping",
                    timeout.as_secs()
                );
                let _ = c.kill().await;
                for ext in ["sock", "pid"] {
                    let _ = std::fs::remove_file(self.stem(&key).with_extension(ext));
                }
                self.conns.lock().unwrap().remove(&key);
            }
        }
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
        self.kill_stale(&pidfile, &socket);
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

    /// Kill a redis-server left by a crashed daemon: only when the pidfile's process
    /// is still a redis-server on this checkout's socket, as the pid may have been
    /// reused (the pidfile outlives reboots).
    fn kill_stale(&self, pidfile: &Path, socket: &Path) {
        if let Ok(pid) = std::fs::read_to_string(pidfile)
            && let Ok(pid) = pid.trim().parse::<i32>()
            && pid > 0
        {
            let names = self.server_names();
            match process_args(pid) {
                Some(args) if ours(&args, &names, socket) => {
                    info!("killing stale redis-server {pid} on {}", socket.display());
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                Some(_) => warn!(
                    "{}: pid {pid} is not our redis-server; leaving it",
                    pidfile.display()
                ),
                None => {}
            }
        }
        let _ = std::fs::remove_file(pidfile);
        let _ = std::fs::remove_file(socket);
    }

    /// Program names a redis-server of ours runs as.
    fn server_names(&self) -> Vec<String> {
        let mut names = vec!["redis-server".to_string()];
        if let Some(n) = self.server.as_deref().and_then(Path::file_name) {
            names.push(n.to_string_lossy().into_owned());
        }
        names
    }

    /// Forget a checkout; stop its redis-server (its data is gone) unless other
    /// checkouts share it.
    pub async fn remove(&self, id: &str) {
        let backend = {
            let mut known = self.known.write().unwrap();
            let Some(b) = known.remove(id) else { return };
            if known.values().any(|o| *o == b) {
                return;
            }
            b
        };
        if let Some(mut c) = self.procs.lock().await.remove(&backend) {
            let _ = c.kill().await;
        }
        for ext in ["sock", "pid", "log"] {
            let _ = std::fs::remove_file(self.stem(&backend).with_extension(ext));
        }
    }

    pub async fn stop_all(&self) {
        for (id, mut c) in self.procs.lock().await.drain() {
            let _ = c.kill().await;
            // SIGKILL leaves them behind.
            for ext in ["sock", "pid"] {
                let _ = std::fs::remove_file(self.stem(&id).with_extension(ext));
            }
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
            let backend = self.known.read().unwrap().get(&id).cloned();
            let Some(backend) = backend else {
                client
                    .write_all(
                        b"-WRONGPASS invalid username-password pair or user is disabled.\r\n",
                    )
                    .await?;
                continue;
            };
            self.activity.touch(&id);
            break (backend, forward);
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
        let _open = OpenConn::new(&self.conns, &id);
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

/// Counts an open client connection of a redis-server while alive.
struct OpenConn<'a> {
    conns: &'a std::sync::Mutex<HashMap<String, (usize, Instant)>>,
    key: String,
}

impl<'a> OpenConn<'a> {
    fn new(conns: &'a std::sync::Mutex<HashMap<String, (usize, Instant)>>, key: &str) -> Self {
        let mut c = conns.lock().unwrap();
        let e = c.entry(key.to_string()).or_insert((0, Instant::now()));
        e.0 += 1;
        e.1 = Instant::now();
        Self {
            conns,
            key: key.to_string(),
        }
    }
}

impl Drop for OpenConn<'_> {
    fn drop(&mut self) {
        if let Some(e) = self.conns.lock().unwrap().get_mut(&self.key) {
            e.0 = e.0.saturating_sub(1);
            e.1 = Instant::now();
        }
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

/// Whether `args` (a process's argv) is a redis-server (`names`) listening on
/// `socket`: as started (`--unixsocket <socket>` as whole argv entries), or with the
/// title redis-server rewrites its argv to, one entry
/// `<argv0> unixsocket:<socket>[ <server mode>]`. Paths may contain spaces.
fn ours(args: &[String], names: &[String], socket: &Path) -> bool {
    let socket = socket.to_string_lossy();
    let prog_ok = |p: &str| {
        Path::new(p)
            .file_name()
            .is_some_and(|n| names.iter().any(|m| **m == *n.to_string_lossy()))
    };
    let Some(first) = args.first() else {
        return false;
    };
    if prog_ok(first) && args[1..].iter().any(|a| *a == *socket) {
        return true;
    }
    const TAG: &str = " unixsocket:";
    first.match_indices(TAG).any(|(i, _)| {
        let rest = &first[i + TAG.len()..];
        prog_ok(&first[..i])
            && rest
                .strip_prefix(&*socket)
                .is_some_and(|r| r.is_empty() || r.starts_with(' '))
    })
}

/// Command line of process `pid`, None if it is gone or unreadable.
#[cfg(target_os = "linux")]
fn process_args(pid: i32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    )
}

/// Command line of process `pid` (sysctl KERN_PROCARGS2), None if it is gone or
/// unreadable.
#[cfg(target_os = "macos")]
fn process_args(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    buf.truncate(size);
    // argc, the executable's path, NUL padding, then argc NUL-terminated arguments.
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?).max(0) as usize;
    let mut parts = buf[4..].split(|b| *b == 0).filter(|a| !a.is_empty());
    parts.next()?;
    Some(
        parts
            .take(argc)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    )
}

/// Unknown platform: never trust a pidfile.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_args(_pid: i32) -> Option<Vec<String>> {
    None
}

pub fn dir() -> PathBuf {
    crate::config::home().join("redis")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_redis_outlives_one_checkout() {
        let d = tempfile::tempdir().unwrap();
        let r = Redis::new(d.path().into(), Default::default(), None);
        r.allow("app", "app+shared");
        r.allow("app--wt", "app+shared");
        r.allow("other", "other");
        std::fs::write(r.stem("app+shared").with_extension("log"), "").unwrap();
        r.remove("app--wt").await;
        // Still used by the primary: kept.
        assert!(r.stem("app+shared").with_extension("log").exists());
        assert_eq!(
            r.known.read().unwrap().get("app").map(String::as_str),
            Some("app+shared")
        );
        r.remove("app").await;
        assert!(!r.stem("app+shared").with_extension("log").exists());
        assert!(r.known.read().unwrap().contains_key("other"));
    }

    #[test]
    fn counts_open_connections() {
        let conns = std::sync::Mutex::new(HashMap::new());
        let a = OpenConn::new(&conns, "k");
        let b = OpenConn::new(&conns, "k");
        assert_eq!(conns.lock().unwrap()["k"].0, 2);
        drop(a);
        drop(b);
        assert_eq!(conns.lock().unwrap()["k"].0, 0);
    }

    #[test]
    fn recognises_our_redis_server() {
        let sock = Path::new("/state/redis/abc.sock");
        let names = ["redis-server".to_string()];
        let args = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        // As started (argv entries), and with its rewritten title (one entry).
        assert!(ours(
            &args("/nix/store/x/bin/redis-server --port 0 --unixsocket /state/redis/abc.sock"),
            &names,
            sock
        ));
        assert!(ours(
            &["/nix/store/x/bin/redis-server unixsocket:/state/redis/abc.sock".to_string()],
            &names,
            sock
        ));
        // Another checkout's, another program, nothing.
        assert!(!ours(
            &["redis-server unixsocket:/state/redis/def.sock".to_string()],
            &names,
            sock
        ));
        assert!(!ours(
            &args("/usr/bin/vim /state/redis/abc.sock"),
            &names,
            sock
        ));
        assert!(!ours(&[], &names, sock));

        // Whole argv entries: a socket path with a space.
        let spaced = Path::new("/Users/me/My State/redis/abc.sock");
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(ours(
            &argv(&[
                "/opt/my bin/redis-server",
                "--unixsocket",
                "/Users/me/My State/redis/abc.sock"
            ]),
            &names,
            spaced
        ));
        assert!(ours(
            &argv(&["redis-server unixsocket:/Users/me/My State/redis/abc.sock "]),
            &names,
            spaced
        ));
        // A prefix of the path, or its pieces as separate words, isn't it.
        assert!(!ours(
            &argv(&["redis-server", "--unixsocket", "/Users/me/My"]),
            &names,
            spaced
        ));
        assert!(!ours(
            &argv(&["redis-server unixsocket:/Users/me/My State/redis/abc.sock2"]),
            &names,
            spaced
        ));
    }

    #[test]
    fn reads_process_args() {
        let me = process_args(std::process::id() as i32).unwrap();
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy();
        assert!(me[0].ends_with(&*name), "{me:?}");
        assert!(process_args(i32::MAX).is_none());
    }

    #[tokio::test]
    async fn stale_pidfile_of_another_process_is_left_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = Redis::new(dir.path().into(), crate::history::Activity::default(), None);
        let pidfile = dir.path().join("x.pid");
        let socket = dir.path().join("x.sock");
        // This test process: not a redis-server, so it survives.
        std::fs::write(&pidfile, std::process::id().to_string()).unwrap();
        std::fs::write(&socket, "").unwrap();
        r.kill_stale(&pidfile, &socket);
        assert!(!pidfile.exists() && !socket.exists());
    }

    #[tokio::test]
    async fn kills_stale_redis_server_of_crashed_daemon() {
        if crate::postgres::which("redis-server").is_none() {
            eprintln!("skipped: no redis-server in PATH");
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let crashed = Redis::new(dir.path().into(), crate::history::Activity::default(), None);
        let socket = crashed.ensure("a").await.unwrap();
        let pidfile = crashed.stem("a").with_extension("pid");
        let mut pid = String::new();
        for _ in 0..250 {
            pid = std::fs::read_to_string(&pidfile).unwrap_or_default();
            if !pid.trim().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid: i32 = pid.trim().parse().unwrap();
        // The daemon dies without killing its child.
        std::mem::forget(crashed);
        let next = Redis::new(dir.path().into(), crate::history::Activity::default(), None);
        next.kill_stale(&pidfile, &socket);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL);
        assert!(!pidfile.exists());
    }

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
