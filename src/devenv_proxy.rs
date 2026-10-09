//! Stands in for devenv's shared `devenv-proxy`: the same control socket and wire
//! protocol, so `devenv up` in a project with `process.proxy.enable` registers its
//! routes here instead of starting a second proxy that can't bind ports 80/443.
//! Requests for those hosts go through lazy-cow-tree's own listeners (proxy.rs).

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use parking_lot::RwLock;
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
    sign::CertifiedKey,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tracing::{info, warn};

const MAX_REQUEST_BYTES: u64 = 64 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// devenv probes the plain HTTP listener with this host and expects a 204.
pub const HEALTH_HOSTNAME: &str = "_devenv-proxy.localhost";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TlsConfig {
    pub certificate: PathBuf,
    pub key: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub hostname: String,
    pub upstream: SocketAddr,
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ControlRequest {
    Status,
    Register { route: Route },
    Unregister { hostname: String, owner: String },
    ReplaceOwner { owner: String, routes: Vec<Route> },
    List,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlResponse {
    Info {
        pid: u32,
        https_listen: Option<SocketAddr>,
    },
    Ok {
        routes: Option<Vec<Route>>,
        removed: Option<bool>,
    },
    Error {
        message: String,
    },
}

struct Registered {
    route: Route,
    certificate: Option<Arc<CertifiedKey>>,
}

impl Registered {
    /// `own_ca`: leave the route's certificate unloaded, lazy-cow-tree's CA serves it.
    fn new(mut route: Route, own_ca: bool) -> Result<Self> {
        route.hostname = normalize_hostname(&route.hostname)?;
        if route.owner.trim().is_empty() {
            bail!("route owner cannot be empty");
        }
        if !route.upstream.ip().is_loopback() {
            bail!("route upstream must use a loopback address");
        }
        let certificate = route
            .tls
            .as_ref()
            .filter(|_| !own_ca)
            .map(|tls| load_certificate(tls, &route.hostname))
            .transpose()?
            .map(Arc::new);
        Ok(Self { route, certificate })
    }
}

/// Whether lazy-cow-tree itself serves a hostname; devenv can't take those.
pub type Reserved = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Routes registered by devenv projects: hostname -> loopback upstream, per owner.
#[derive(Clone)]
pub struct DevenvRoutes {
    map: Arc<RwLock<BTreeMap<String, Arc<Registered>>>>,
    reserved: Reserved,
    https_listen: Option<SocketAddr>,
    own_ca: bool,
}

impl std::fmt::Debug for DevenvRoutes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DevenvRoutes")
    }
}

impl Default for DevenvRoutes {
    fn default() -> Self {
        Self::new(Arc::new(|_| false), None, false)
    }
}

impl DevenvRoutes {
    pub fn new(reserved: Reserved, https_listen: Option<SocketAddr>, own_ca: bool) -> Self {
        Self {
            map: Default::default(),
            reserved,
            https_listen,
            own_ca,
        }
    }

    fn check_free(
        &self,
        map: &BTreeMap<String, Arc<Registered>>,
        hostname: &str,
        owner: &str,
    ) -> Result<()> {
        if (self.reserved)(hostname) {
            bail!("hostname {hostname} is served by lazy-cow-tree");
        }
        if map
            .get(hostname)
            .is_some_and(|existing| existing.route.owner != owner)
        {
            bail!("hostname {hostname} is already owned by another project");
        }
        Ok(())
    }

    pub fn register(&self, route: Route) -> Result<()> {
        let registered = Registered::new(route, self.own_ca)?;
        let mut map = self.map.write();
        self.check_free(&map, &registered.route.hostname, &registered.route.owner)?;
        map.insert(registered.route.hostname.clone(), Arc::new(registered));
        Ok(())
    }

    pub fn unregister(&self, hostname: &str, owner: &str) -> Result<bool> {
        let hostname = normalize_hostname(hostname)?;
        let mut map = self.map.write();
        let Some(existing) = map.get(&hostname) else {
            return Ok(false);
        };
        if existing.route.owner != owner {
            bail!("hostname {hostname} is owned by another project");
        }
        map.remove(&hostname);
        Ok(true)
    }

    /// Atomically replace every route of `owner`: `devenv up` reconciles with this,
    /// dropping routes that vanished since the last evaluation.
    pub fn replace_owner(&self, owner: &str, routes: Vec<Route>) -> Result<()> {
        if owner.trim().is_empty() {
            bail!("route owner cannot be empty");
        }
        let mut replacement = BTreeMap::new();
        for route in routes {
            if route.owner != owner {
                bail!("replacement route has a different owner");
            }
            let registered = Registered::new(route, self.own_ca)?;
            if replacement
                .insert(registered.route.hostname.clone(), Arc::new(registered))
                .is_some()
            {
                bail!("replacement contains a duplicate hostname");
            }
        }
        let mut map = self.map.write();
        for hostname in replacement.keys() {
            self.check_free(&map, hostname, owner)?;
        }
        map.retain(|_, r| r.route.owner != owner);
        map.extend(replacement);
        Ok(())
    }

    pub fn resolve(&self, hostname: &str) -> Option<SocketAddr> {
        let hostname = normalize_hostname(hostname).ok()?;
        self.map.read().get(&hostname).map(|r| r.route.upstream)
    }

    pub fn certificate(&self, hostname: &str) -> Option<Arc<CertifiedKey>> {
        let hostname = normalize_hostname(hostname).ok()?;
        self.map.read().get(&hostname)?.certificate.clone()
    }

    pub fn list(&self) -> Vec<Route> {
        self.map.read().values().map(|r| r.route.clone()).collect()
    }

    fn dispatch(&self, request: ControlRequest) -> Result<ControlResponse> {
        let ok = |routes, removed| ControlResponse::Ok { routes, removed };
        Ok(match request {
            ControlRequest::Status => ControlResponse::Info {
                pid: std::process::id(),
                https_listen: self.https_listen,
            },
            ControlRequest::Register { route } => {
                self.register(route)?;
                ok(None, None)
            }
            ControlRequest::Unregister { hostname, owner } => {
                ok(None, Some(self.unregister(&hostname, &owner)?))
            }
            ControlRequest::ReplaceOwner { owner, routes } => {
                self.replace_owner(&owner, routes)?;
                ok(None, None)
            }
            ControlRequest::List => ok(Some(self.list()), None),
        })
    }
}

/// devenv's `.localhost` hostname rules, so both accept and refuse the same names.
pub fn normalize_hostname(hostname: &str) -> Result<String> {
    let hostname = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
    if hostname != "localhost" && !hostname.ends_with(".localhost") {
        bail!("route hostname must be localhost or end in .localhost");
    }
    if hostname
        .bytes()
        .any(|b| !(b.is_ascii_alphanumeric() || b == b'-' || b == b'.'))
    {
        bail!("route hostname contains invalid characters");
    }
    if hostname
        .split('.')
        .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
    {
        bail!("route hostname contains an invalid label");
    }
    Ok(hostname)
}

/// The project's mkcert certificate for `hostname`, served instead of lazy-cow-tree's
/// CA: the browser trusts that project's CA for it.
fn load_certificate(tls: &TlsConfig, hostname: &str) -> Result<CertifiedKey> {
    let chain = CertificateDer::pem_file_iter(&tls.certificate)
        .with_context(|| format!("failed to read certificate {}", tls.certificate.display()))?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to parse proxy certificate")?;
    let Some(leaf) = chain.first() else {
        bail!("proxy certificate file is empty");
    };
    let name = ServerName::try_from(hostname.to_string())?;
    webpki::EndEntityCert::try_from(leaf)
        .context("failed to parse proxy certificate")?
        .verify_is_valid_for_subject_name(&name)
        .map_err(|_| anyhow::anyhow!("proxy certificate does not cover {hostname}"))?;
    let key = PrivateKeyDer::from_pem_file(&tls.key)
        .with_context(|| format!("failed to read certificate key {}", tls.key.display()))?;
    let signing = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key)
        .context("unsupported certificate key")?;
    let certified = CertifiedKey::new(chain, signing);
    certified
        .keys_match()
        .context("proxy certificate and key do not match")?;
    Ok(certified)
}

/// devenv's path for the per-user control socket, so `devenv up` finds this one.
pub fn default_control_socket() -> PathBuf {
    if let Some(path) = std::env::var_os("DEVENV_PROXY_SOCKET") {
        return path.into();
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("devenv/proxy.sock");
    }
    std::env::temp_dir().join(format!(
        "devenv-proxy-{}.sock",
        username().replace(['/', '\\'], "-")
    ))
}

fn username() -> String {
    // SAFETY: getpwuid returns a pointer into static storage or null; the name is
    // copied out before any other call could overwrite it.
    let from_passwd = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        (!pw.is_null() && !(*pw).pw_name.is_null()).then(|| {
            std::ffi::CStr::from_ptr((*pw).pw_name)
                .to_string_lossy()
                .into_owned()
        })
    };
    from_passwd
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Serve the control socket. Refuses when another proxy (devenv's own) answers on it.
pub async fn serve(socket: &Path, routes: DevenvRoutes) -> Result<()> {
    prepare_socket(socket).await?;
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("failed to bind proxy control socket {}", socket.display()))?;
    std::fs::set_permissions(socket, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .context("failed to secure proxy control socket")?;
    info!("devenv proxy control socket on {}", socket.display());
    loop {
        let (stream, _) = listener.accept().await?;
        let routes = routes.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &routes).await {
                warn!("devenv proxy control: {e:#}");
            }
        });
    }
}

async fn prepare_socket(socket: &Path) -> Result<()> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let Ok(metadata) = std::fs::symlink_metadata(socket) else {
        return Ok(());
    };
    if !std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type()) {
        bail!("refusing to replace non-socket path {}", socket.display());
    }
    if UnixStream::connect(socket).await.is_ok() {
        bail!(
            "a proxy is already listening on {}; stop devenv's own proxy so lazy-cow-tree can serve its routes",
            socket.display()
        );
    }
    std::fs::remove_file(socket)
        .with_context(|| format!("failed to remove stale socket {}", socket.display()))
}

async fn handle(mut stream: UnixStream, routes: &DevenvRoutes) -> Result<()> {
    let (read, mut write) = stream.split();
    let mut line = String::new();
    tokio::time::timeout(
        CONTROL_TIMEOUT,
        BufReader::new(read.take(MAX_REQUEST_BYTES)).read_line(&mut line),
    )
    .await
    .context("proxy request timed out")??;
    let response = if line.is_empty() {
        Err(anyhow::anyhow!("empty proxy request"))
    } else {
        serde_json::from_str(&line)
            .context("invalid proxy request")
            .and_then(|request| routes.dispatch(request))
    }
    .unwrap_or_else(|e| ControlResponse::Error {
        message: format!("{e:#}"),
    });
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    write.write_all(&out).await?;
    Ok(())
}

/// The loopback address to connect to: `upstream`, else the other loopback family
/// (dev servers bound to "localhost" listen on either).
pub async fn reachable(upstream: SocketAddr) -> SocketAddr {
    let connects = |addr| async move {
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        .is_ok_and(|r| r.is_ok())
    };
    if connects(upstream).await {
        return upstream;
    }
    let other = match upstream.ip() {
        IpAddr::V4(ip) if ip == Ipv4Addr::LOCALHOST => IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::V6(ip) if ip == Ipv6Addr::LOCALHOST => IpAddr::V4(Ipv4Addr::LOCALHOST),
        _ => return upstream,
    };
    let other = SocketAddr::new(other, upstream.port());
    if connects(other).await {
        other
    } else {
        upstream
    }
}

#[cfg(test)]
mod tests;
