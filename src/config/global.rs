//! Daemon-wide settings (`Global`): ports, PostgreSQL and Redis, from flags or
//! LAZY_COW_TREE_* variables.

use super::*;

/// Lowest port an unprivileged process may bind: 0 on macOS (for all interfaces),
/// `net.ipv4.ip_unprivileged_port_start` on Linux (1024 unless lowered, e.g.
/// `sysctl net.ipv4.ip_unprivileged_port_start=80`).
fn unprivileged_port_start() -> u16 {
    if cfg!(target_os = "macos") {
        return 0;
    }
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1024)
}

/// 443 where it may be bound without root, else 8443.
fn default_https_port() -> u16 {
    if unprivileged_port_start() <= 443 {
        443
    } else {
        8443
    }
}

fn default_role_connections() -> u32 {
    200
}

/// 80 where it may be bound without root, else off.
fn default_http_port() -> u16 {
    if unprivileged_port_start() <= 80 {
        80
    } else {
        0
    }
}

/// Daemon-wide settings, shared by every project.
#[derive(clap::Args, Debug, Clone, Serialize, Deserialize)]
pub struct Global {
    /// PostgreSQL port on 127.0.0.1 (lazy-cow-tree's proxy; the user picks the checkout).
    /// The real server listens on <state>/pg/.s.PGSQL.<port + 1> only.
    #[arg(
        long,
        env = "LAZY_COW_TREE_PG_PORT",
        default_value_t = 55432,
        global = true
    )]
    pub pg_port: u16,
    /// Redis port on 127.0.0.1. The password picks the checkout: each has its own
    /// redis-server behind it (REDIS_URL in the devenv shell).
    #[arg(
        long,
        env = "LAZY_COW_TREE_REDIS_PORT",
        default_value_t = 6380,
        global = true
    )]
    pub redis_port: u16,
    /// HTTPS proxy port [default: 443 where unprivileged processes may bind it (macOS;
    /// Linux with net.ipv4.ip_unprivileged_port_start <= 443), else 8443]. Below 1024
    /// it binds all interfaces and refuses non-loopback peers.
    #[arg(
        long,
        env = "LAZY_COW_TREE_HTTPS_PORT",
        default_value_t = default_https_port(),
        global = true
    )]
    pub https_port: u16,
    /// Plain HTTP port that redirects to HTTPS; 0 disables [default: 80 where
    /// unprivileged processes may bind it, else off].
    #[arg(
        long,
        env = "LAZY_COW_TREE_HTTP_PORT",
        default_value_t = default_http_port(),
        global = true
    )]
    pub http_port: u16,
    /// PostgreSQL RAM disk size in MB (memory is only used as it fills).
    #[arg(
        long,
        env = "LAZY_COW_TREE_RAMDISK_MB",
        default_value_t = 4096,
        global = true
    )]
    pub ramdisk_mb: u64,
    /// Directory with PostgreSQL's `postgres` and `initdb` [default: from PATH].
    #[arg(long, env = "LAZY_COW_TREE_POSTGRES_BIN", global = true)]
    pub postgres_bin: Option<PathBuf>,
    /// Extra postgresql.conf settings as a JSON object, e.g.
    /// {"shared_preload_libraries": "pg_stat_statements"}.
    #[arg(long, env = "LAZY_COW_TREE_POSTGRES_SETTINGS", global = true)]
    pub postgres_settings: Option<String>,
    /// Extensions to create in template1 (so in every database made afterwards) and
    /// the primaries' databases, as superuser: for extensions that aren't trusted
    /// (postgis, vector), which checkout roles can't create. Comma or space separated.
    #[arg(long, env = "LAZY_COW_TREE_POSTGRES_EXTENSIONS", global = true)]
    pub postgres_extensions: Option<String>,
    /// The `redis-server` to run [default: from PATH].
    #[arg(long, env = "LAZY_COW_TREE_REDIS_SERVER", global = true)]
    pub redis_server: Option<PathBuf>,
    /// Control socket where devenv projects (`process.proxy.enable`) register their
    /// hostnames, as with devenv's own proxy; "off" disables [default: devenv's path,
    /// $DEVENV_PROXY_SOCKET or $TMPDIR/devenv-proxy-<user>.sock].
    #[arg(long, env = "LAZY_COW_TREE_DEVENV_PROXY_SOCKET", global = true)]
    #[serde(default)]
    pub devenv_proxy_socket: Option<PathBuf>,
    /// Serve the hostnames devenv projects register with lazy-cow-tree's CA (trusted
    /// once, `lazy-cow-tree trust`) instead of each project's own mkcert certificate.
    /// With TRUST_STORES=none mkcert then never asks to trust a new project's CA.
    #[arg(long, env = "LAZY_COW_TREE_DEVENV_PROXY_CA", global = true, default_value_t = false, value_parser = clap::builder::BoolishValueParser::new())]
    #[serde(default)]
    pub devenv_proxy_ca: bool,
    /// PostgreSQL keeps its data safe (on disk, fsync on) instead of the RAM disk with
    /// fsync, synchronous_commit and full_page_writes off. Daemon-wide: projects with
    /// another value are refused.
    #[arg(long, env = "LAZY_COW_TREE_POSTGRES_DURABLE", global = true, default_value_t = false, value_parser = clap::builder::BoolishValueParser::new())]
    #[serde(default)]
    pub postgres_durable: bool,
    /// Connections one checkout's role may hold (CONNECTION LIMIT), so one test suite
    /// can't take the whole cluster's (max_connections 1000) from the others.
    #[arg(long, env = "LAZY_COW_TREE_POSTGRES_ROLE_CONNECTIONS", global = true, default_value_t = default_role_connections())]
    #[serde(default = "default_role_connections")]
    pub postgres_role_connections: u32,
    /// Stop a per-checkout redis-server after this many seconds without connections
    /// (it starts again on the next one).
    #[arg(long, env = "LAZY_COW_TREE_REDIS_IDLE_TIMEOUT", global = true)]
    #[serde(default)]
    pub redis_idle_timeout: Option<u64>,
}

impl Global {
    /// `--postgres-extensions` as names.
    pub fn postgres_extensions(&self) -> Vec<String> {
        self.postgres_extensions
            .as_deref()
            .unwrap_or_default()
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// `postgres_settings` as `name=value` pairs.
    pub fn postgres_settings(&self) -> Result<Vec<(String, String)>> {
        let Some(json) = self
            .postgres_settings
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        else {
            return Ok(Vec::new());
        };
        let map: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(json).context("LAZY_COW_TREE_POSTGRES_SETTINGS")?;
        Ok(map
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    serde_json::Value::Bool(b) => if b { "on" } else { "off" }.to_string(),
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                (k, v)
            })
            .collect())
    }
}
