//! Paths, settings and the naming scheme every part of the service shares. Names and
//! ports are derived from a checkout's path alone, so the shell hook in a worktree
//! computes the same values as the daemon without asking it.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use git2::Repository;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod env;
mod services;
mod state;
mod tls;

pub use self::env::*;
pub use self::services::*;
pub use self::state::*;
pub use self::tls::*;

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

/// Per-project settings, sent by `lazy-cow-tree serve` when it registers a project.
#[derive(clap::Args, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectSettings {
    /// Project name: <name>.localhost, <worktree>.<name>.localhost, database prefix
    /// [default: primary checkout's directory name].
    #[arg(long = "project", env = "LAZY_COW_TREE_PROJECT")]
    pub name: Option<String>,
    /// Port the primary checkout's app listens on (worktrees get their own).
    #[arg(long, env = "LAZY_COW_TREE_PORT", default_value_t = 4000)]
    pub port: u16,
    /// Git remote to fetch from and watch on GitHub.
    #[arg(long, env = "LAZY_COW_TREE_REMOTE", default_value = "origin")]
    pub remote: String,
    /// Base branch [default: remote HEAD, else main].
    #[arg(long, env = "LAZY_COW_TREE_BASE")]
    pub base: Option<String>,
    /// Where new worktrees go, relative to the primary checkout.
    #[arg(
        long,
        env = "LAZY_COW_TREE_WORKTREES_DIR",
        default_value = ".claude/worktrees"
    )]
    pub worktrees_dir: PathBuf,
    /// Migrate/seed command, run with the checkout's env: in the primary checkout when
    /// the base branch moves (then the template database is refreshed from it), and in
    /// every worktree the base branch was merged into. Split like a shell would, but
    /// run directly, e.g. "mix do ecto.migrate + run priv/repo/seeds.exs".
    #[arg(long, env = "LAZY_COW_TREE_MIGRATE")]
    pub migrate: Option<String>,
    /// Seed command, run in the primary checkout after `migrate` when its database
    /// was just created (empty); worktrees get the seeded data through the template.
    /// E.g. "mix run priv/repo/seeds.exs".
    #[arg(long, env = "LAZY_COW_TREE_SEED")]
    pub seed: Option<String>,
    /// Setup command, run once in every new checkout (made by lazy-cow-tree, git,
    /// git-cow or Claude Code) with its env, before its services start, e.g.
    /// "mix deps.get". Checkouts it ran in are marked in their git admin dir.
    #[arg(long, env = "LAZY_COW_TREE_SETUP")]
    pub setup: Option<String>,
    /// Services of every checkout as JSON ({"web": {"exec": "mix phx.server"}, ...};
    /// see `Service`), written by the devenv module's `lazy-cow-tree.services`. Each is
    /// started on demand (its first request, or as a dependency) with its own port and
    /// hostname, sharing the checkout's database and Redis.
    #[arg(long, env = "LAZY_COW_TREE_SERVICES", default_value = "{}")]
    pub services: Services,
    /// Don't pull branches / merge the base branch into worktrees on pushes.
    #[arg(long, env = "LAZY_COW_TREE_NO_SYNC", value_parser = clap::builder::BoolishValueParser::new())]
    pub no_sync: bool,
    /// Don't remove worktrees whose PR merged.
    #[arg(long, env = "LAZY_COW_TREE_NO_AUTO_REMOVE", value_parser = clap::builder::BoolishValueParser::new())]
    pub no_auto_remove: bool,
    /// More databases every checkout gets, next to its main one (e.g. `cms`: a second
    /// Ecto repo): `<prefix>_<name>_dev`, `_test` and test partitions per checkout,
    /// worktrees' cloned from `<prefix>_<name>_template`, in `<NAME>_DATABASE_URL` and
    /// `<NAME>_TEST_DATABASE_URL`. Names: lowercase letters, digits and `_`.
    #[arg(long, env = "LAZY_COW_TREE_DATABASES", value_delimiter = ',', value_parser = parse_db_name)]
    #[serde(default)]
    pub databases: Vec<String>,
    /// Serve hostnames under this domain (`<worktree>.<service>.<project>.<domain>`,
    /// its `*` A record pointing at 127.0.0.1) with the certificate in the GitHub
    /// repository's variables (`tls::trusted`), instead of `.localhost` ones with the
    /// local CA.
    #[arg(long, env = "LAZY_COW_TREE_TLS_DOMAIN", value_parser = parse_domain)]
    #[serde(default)]
    pub tls_domain: Option<String>,
    /// GitHub repository whose `https-certificate` artifact has the domain's
    /// certificates (`owner/repo`, `host/owner/repo` or a URL), when it isn't the
    /// checkout's `remote`: e.g. one repository issuing them for several projects.
    #[arg(long, env = "LAZY_COW_TREE_TLS_GITHUB_REPOSITORY", value_parser = parse_repository)]
    #[serde(default)]
    pub tls_github_repository: Option<String>,
    /// Service names the certificate covers besides the project's own services, so
    /// adding one of them needs no new certificate.
    #[arg(
        long,
        env = "LAZY_COW_TREE_TLS_SERVICES",
        value_delimiter = ',',
        default_value = TLS_SERVICES
    )]
    #[serde(default = "default_tls_services")]
    pub tls_services: Vec<String>,
}

/// Lowercase DNS label of at most 32 characters (also fits database names).
pub fn dns_label(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(32).collect();
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "x".into() } else { out }
}

/// Name of a worktree, from its git admin directory name (`.git/worktrees/<name>`),
/// which is unique in the repository: git names it after the worktree's directory and
/// numbers repeats. Kept as is when it is already a DNS label; otherwise normalized,
/// shortened and suffixed with a hash of the original, so two worktrees never share a
/// name (and with it hostnames, databases, role, Redis and ports).
pub fn worktree_label(name: &str) -> String {
    if valid_label(name) {
        return name.to_string();
    }
    let hash = hex::encode(&Sha256::digest(name.as_bytes())[..3]);
    let base: String = dns_label(name).chars().take(32 - 1 - hash.len()).collect();
    format!("{}-{hash}", base.trim_end_matches('-'))
}

/// `ProjectSettings::databases` entry: part of database names and variable names.
fn parse_db_name(s: &str) -> std::result::Result<String, String> {
    if !s.is_empty()
        && s.len() <= 20
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        Ok(s.to_string())
    } else {
        Err(format!(
            "{s:?}: lowercase letters, digits and _, starting with a letter, at most 20"
        ))
    }
}

pub fn valid_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// A registered project: its primary checkout and settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub settings: ProjectSettings,
    /// Environment of the `lazy-cow-tree serve` that registered it (PATH for the migrate
    /// and service commands, the project's toolchain).
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

impl Project {
    pub fn new(root: PathBuf, settings: ProjectSettings) -> Self {
        let name = settings.name.as_deref().map_or_else(
            || dns_label(&root.file_name().unwrap_or_default().to_string_lossy()),
            dns_label,
        );
        Self {
            root,
            name,
            settings,
            env: Vec::new(),
        }
    }

    pub fn db_prefix(&self) -> String {
        self.name.replace('-', "_")
    }

    /// A variable of the registering `lazy-cow-tree serve`'s environment.
    pub fn env_var(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    fn env_flag(&self, name: &str, default: bool) -> bool {
        match self.env_var(name) {
            Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            None => default,
        }
    }

    /// `LAZY_COW_TREE_POSTGRES_DURABLE` of this project (unset: not durable).
    pub fn postgres_durable(&self) -> bool {
        self.env_flag("LAZY_COW_TREE_POSTGRES_DURABLE", false)
    }

    /// `LAZY_COW_TREE_POSTGRES_COW`: worktree databases are clones of the template
    /// (default), else created empty and migrated.
    pub fn copy_on_write(&self) -> bool {
        self.env_flag("LAZY_COW_TREE_POSTGRES_COW", true)
    }

    /// `LAZY_COW_TREE_POSTGRES_TEMPLATE_REFRESH=manual`: base-branch moves don't
    /// refresh the template; `lazy-cow-tree snapshot` does.
    pub fn template_refresh_manual(&self) -> bool {
        self.env_var("LAZY_COW_TREE_POSTGRES_TEMPLATE_REFRESH") == Some("manual")
    }

    /// `LAZY_COW_TREE_REDIS_START=up`: a checkout's redis-server starts with it.
    pub fn redis_start_up(&self) -> bool {
        self.env_var("LAZY_COW_TREE_REDIS_START") == Some("up")
    }

    /// `LAZY_COW_TREE_REDIS_INSTANCE=shared`: one redis-server for all its checkouts.
    pub fn redis_shared(&self) -> bool {
        self.env_var("LAZY_COW_TREE_REDIS_INSTANCE") == Some("shared")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join(&self.settings.worktrees_dir)
    }

    /// A worktree's port is the one recorded in its git admin dir (the daemon records
    /// them, `worktree::assign_ports`), else its hashed slot; the primary's is the one
    /// the daemon recorded for its setting (`worktree::assign_primary_port`), else the
    /// setting. Reads only: the shell hook computes an unrecorded worktree's with
    /// `worktree::plan_ports` first.
    pub fn checkout(&self, worktree: Option<&str>, path: PathBuf) -> Checkout {
        let port = match worktree {
            None => crate::worktree::primary_port(&self.root, self.settings.port),
            Some(w) => crate::worktree::recorded_port(&path)
                .unwrap_or_else(|| worktree_port(&self.name, w)),
        };
        self.checkout_on(worktree, path, port)
    }

    /// `checkout` with a given base port.
    pub fn checkout_on(&self, worktree: Option<&str>, path: PathBuf, port: u16) -> Checkout {
        Checkout {
            project: self.name.clone(),
            db_prefix: self.db_prefix(),
            worktree: worktree.map(str::to_string),
            path,
            port,
            services: self.settings.services.clone(),
            extra_dbs: self.settings.databases.clone(),
            domain: self.settings.tls_domain.clone(),
        }
    }
}

/// devenv's `strict_ports` (top level of devenv.yaml; devenv.local.yaml wins): a taken
/// port is an error instead of a move to the next free one.
pub fn strict_ports(root: &Path) -> bool {
    ["devenv.yaml", "devenv.local.yaml"]
        .iter()
        .fold(false, |acc, f| {
            std::fs::read_to_string(root.join(f))
                .ok()
                .and_then(|t| {
                    t.lines().find_map(|l| {
                        l.strip_prefix("strict_ports:")
                            .map(|v| v.split('#').next().unwrap_or("").trim() == "true")
                    })
                })
                .unwrap_or(acc)
        })
}

/// Worktree base ports: 900 slots of 10 (a worktree's services use its base port and
/// the 9 above it).
pub const WORKTREE_PORTS: std::ops::Range<u16> = 20000..29000;

/// A worktree's first-choice slot, from a hash of project and worktree. Its actual
/// port is recorded in its git admin dir by `worktree::port`, which moves on to the
/// next free slot when another worktree has this one.
pub fn worktree_port(project: &str, worktree: &str) -> u16 {
    let h = Sha256::digest(format!("{project}/{worktree}").as_bytes());
    let slots = u32::from(WORKTREE_PORTS.end - WORKTREE_PORTS.start) / 10;
    WORKTREE_PORTS.start + (u32::from_be_bytes([h[0], h[1], h[2], h[3]]) % slots) as u16 * 10
}

/// PostgreSQL's longest name (NAMEDATALEN - 1); it silently truncates longer ones.
pub const PG_NAME_MAX: usize = 63;

/// Longest infix a checkout's database names get: `_test_p<N>` of test partitions.
const LONGEST_DB_INFIX: &str = "_test_p9999";

fn short_hash(s: &str) -> String {
    hex::encode(&Sha256::digest(s.as_bytes())[..4])
}

/// Every database and role name lazy-cow-tree makes: kept when it fits PostgreSQL's 63
/// bytes, else cut and suffixed with a hash of the whole, so two long names can't
/// truncate to the same one.
pub fn pg_name(s: &str) -> String {
    if s.len() <= PG_NAME_MAX {
        return s.to_string();
    }
    let hash = short_hash(s);
    let mut head = String::new();
    for c in s.chars() {
        if head.len() + c.len_utf8() > PG_NAME_MAX - hash.len() - 1 {
            break;
        }
        head.push(c);
    }
    format!("{head}_{hash}")
}

/// A worktree's databases and role under its pre-upgrade name (`Checkout::legacy`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy {
    pub name: String,
    pub role: String,
    /// (old, new) dev and test databases.
    pub dbs: [(String, String); 2],
    test_prefix: String,
    suffix: String,
}

impl Legacy {
    /// Test partitions under the old name (dropped: tests recreate them).
    pub fn owns_partition(&self, db: &str) -> bool {
        let Some(mid) = db
            .strip_prefix(&self.test_prefix)
            .and_then(|r| r.strip_suffix(&self.suffix))
        else {
            return false;
        };
        let mid = mid.trim_start_matches('_').trim_start_matches('p');
        !mid.is_empty() && mid.bytes().all(|b| b.is_ascii_digit())
    }
}

/// One checkout (primary or worktree) of a project, with everything derived from it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Checkout {
    pub project: String,
    pub db_prefix: String,
    /// None for the primary checkout.
    pub worktree: Option<String>,
    pub path: PathBuf,
    /// Base port: the default service's (or the app's without services).
    pub port: u16,
    #[serde(default)]
    pub services: Services,
    /// Names of its databases besides the main one (`ProjectSettings::databases`).
    #[serde(default)]
    pub extra_dbs: Vec<String>,
    /// Hostnames end in it instead of `localhost` (`ProjectSettings::tls_domain`).
    #[serde(default)]
    pub domain: Option<String>,
}

impl Checkout {
    /// `localhost`, or the project's own domain.
    pub fn domain(&self) -> &str {
        self.domain.as_deref().unwrap_or("localhost")
    }

    pub fn host(&self) -> String {
        let d = self.domain();
        match &self.worktree {
            Some(w) => format!("{w}.{}.{d}", self.project),
            None => format!("{}.{d}", self.project),
        }
    }

    /// `_<worktree>` of its database names, shortened (with a hash of the worktree)
    /// where the longest name built from it, a `<prefix>_test_p<NNNN>_<wt>` partition,
    /// would pass PostgreSQL's 63 bytes: its truncation would cut the end, where
    /// worktrees differ. Extra databases' `_<name>` counts too; without them names
    /// are as before.
    fn suffix(&self) -> String {
        let Some(w) = self.worktree.as_deref() else {
            return String::new();
        };
        let full = format!("_{}", w.replace('-', "_"));
        let longest_extra = self
            .extra_dbs
            .iter()
            .map(|n| n.len() + 1)
            .max()
            .unwrap_or(0);
        let room = PG_NAME_MAX
            .saturating_sub(self.db_prefix.len() + longest_extra + LONGEST_DB_INFIX.len());
        if full.len() <= room {
            return full;
        }
        let hash = short_hash(w);
        let keep = room.saturating_sub(hash.len() + 1).max(1);
        let head: String = full.chars().take(keep).collect();
        format!("{}_{hash}", head.trim_end_matches('_'))
    }

    pub fn service(&self, name: &str) -> Option<&Service> {
        self.services.0.get(name)
    }

    pub fn service_port(&self, name: &str) -> u16 {
        self.port + self.services.offset(name)
    }

    /// `<worktree>.<service>.<project>.localhost`, `<service>.<project>.localhost` in
    /// the primary checkout (the project's domain instead of `localhost` when set).
    pub fn service_host(&self, name: &str) -> String {
        let base = self
            .service(name)
            .and_then(|s| s.hostname.clone())
            .unwrap_or_else(|| format!("{name}.{}.{}", self.project, self.domain()));
        match &self.worktree {
            Some(w) => format!("{w}.{base}"),
            None => base,
        }
    }

    /// (service, host, port) of every http service; without services the checkout's
    /// own host and port.
    pub fn routes(&self) -> Vec<(Option<String>, String, u16)> {
        if self.services.0.is_empty() {
            return vec![(None, self.host(), self.port)];
        }
        self.services
            .0
            .iter()
            .filter(|(_, s)| s.http)
            .map(|(n, _)| (Some(n.clone()), self.service_host(n), self.service_port(n)))
            .chain(self.port_slots().into_iter().filter(|p| p.http).map(|p| {
                (
                    Some(p.service),
                    self.service_host(&p.name),
                    self.port + p.offset,
                )
            }))
            .collect()
    }

    /// Every port the checkout listens on: its services' and their secondary ports
    /// (the base port alone without services).
    pub fn used_ports(&self) -> Vec<u16> {
        match self.services.layout() {
            Ok(l) if !l.services.is_empty() => l
                .services
                .values()
                .chain(l.ports.iter().map(|p| &p.offset))
                .map(|o| self.port + o)
                .collect(),
            _ => vec![self.port],
        }
    }

    /// The services' secondary ports.
    pub fn port_slots(&self) -> Vec<PortSlot> {
        self.services.layout().map(|l| l.ports).unwrap_or_default()
    }

    /// Host of the default service (the checkout's own without services).
    pub fn main_host(&self) -> String {
        match self.services.default_name() {
            Some(d) => self.service_host(d),
            None => self.host(),
        }
    }

    /// `<prefix>` of the main databases, `<prefix>_<name>` of extra database `name`.
    fn kind_prefix(&self, kind: Option<&str>) -> String {
        match kind {
            None => self.db_prefix.clone(),
            Some(k) => format!("{}_{k}", self.db_prefix),
        }
    }

    /// The main database (None) and each extra one (`ProjectSettings::databases`).
    pub fn db_kinds(&self) -> impl Iterator<Item = Option<&str>> {
        std::iter::once(None).chain(self.extra_dbs.iter().map(|k| Some(k.as_str())))
    }

    pub fn dev_db(&self) -> String {
        self.dev_db_of(None)
    }

    pub fn test_db(&self) -> String {
        self.test_db_of(None)
    }

    pub fn dev_db_of(&self, kind: Option<&str>) -> String {
        pg_name(&format!("{}_dev{}", self.kind_prefix(kind), self.suffix()))
    }

    pub fn test_db_of(&self, kind: Option<&str>) -> String {
        pg_name(&format!("{}_test{}", self.kind_prefix(kind), self.suffix()))
    }

    /// The project's template database of `kind`, which worktrees' dev databases of
    /// that kind are cloned from.
    pub fn template_db_of(&self, kind: Option<&str>) -> String {
        pg_name(&format!("{}_template", self.kind_prefix(kind)))
    }

    /// Its dev databases, the main one first.
    pub fn dev_dbs(&self) -> Vec<String> {
        self.db_kinds().map(|k| self.dev_db_of(k)).collect()
    }

    /// Databases this checkout may own: `owns_db`.
    pub fn owns_db(&self, db: &str) -> bool {
        self.db_claim(db).is_some()
    }

    /// Whether this checkout may own `db`, and how closely: (an exact dev or test
    /// name, length of the test database a partition extends) for `db_owner`. Per
    /// kind (main and extra): dev, test, and MIX_TEST_PARTITION ones, `<test db><N>`
    /// (Ecto's usual `"..._test#{partition}"` on TEST_DATABASE_URL) or
    /// `<prefix>_test<N>_<worktree>`. No other form: `<prefix>_test_<N>_<wt>` and
    /// `<prefix>_test_p<N>_<wt>` are test databases of worktrees `<N>-<wt>` and
    /// `p<N>-<wt>`. `<test db><N>` still overlaps worktrees named like this one plus
    /// digits (`x` + 2 vs worktree `x2`): `db_owner` settles those.
    pub fn db_claim(&self, db: &str) -> Option<(bool, usize)> {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let suffix = self.suffix();
        self.db_kinds()
            .filter_map(|kind| {
                let (dev, test) = (self.dev_db_of(kind), self.test_db_of(kind));
                if db == dev || db == test {
                    return Some((true, test.len()));
                }
                if db.strip_prefix(&test).is_some_and(digits)
                    // Worktrees only: for the primary it's `<test db><N>` again.
                    || (!suffix.is_empty()
                        && db
                            .strip_prefix(&format!("{}_test", self.kind_prefix(kind)))
                            .and_then(|rest| rest.strip_suffix(&suffix))
                            .is_some_and(digits))
                {
                    return Some((false, test.len()));
                }
                None
            })
            .max()
    }

    /// The same checkout (project and worktree), whatever its settings.
    pub fn same(&self, other: &Checkout) -> bool {
        (&self.project, &self.worktree) == (&other.project, &other.worktree)
    }

    /// Key for per-checkout processes and logs; also its PostgreSQL role and the
    /// Redis password that picks its redis-server behind the shared Redis port.
    /// `<project>--<worktree>`: project names never contain `--` (`dns_label`), so
    /// project `shop` + worktree `admin-foo` and project `shop-admin` + worktree `foo`
    /// stay apart. Over PostgreSQL's 63-byte names, shortened with a hash of the whole.
    pub fn id(&self) -> String {
        match &self.worktree {
            Some(w) => pg_name(&format!("{}--{w}", self.project)),
            None => self.project.clone(),
        }
    }

    /// Databases and role of this worktree under its name before worktree names
    /// came from git admin dirs (`dns_label` of its directory), when that differs:
    /// (old name, old role, (old, new) dev and test databases).
    pub fn legacy(&self) -> Option<Legacy> {
        let w = self.worktree.as_deref()?;
        let old = dns_label(&self.path.file_name()?.to_string_lossy());
        if old == w {
            return None;
        }
        // The old code didn't shorten names; PostgreSQL truncated them.
        let trunc = |s: String| s.chars().take(PG_NAME_MAX).collect::<String>();
        let suffix = format!("_{}", old.replace('-', "_"));
        Some(Legacy {
            dbs: [
                (
                    trunc(format!("{}_dev{suffix}", self.db_prefix)),
                    self.dev_db(),
                ),
                (
                    trunc(format!("{}_test{suffix}", self.db_prefix)),
                    self.test_db(),
                ),
            ],
            role: trunc(format!("{}-{old}", self.project)),
            test_prefix: format!("{}_test", self.db_prefix),
            suffix,
            name: old,
        })
    }

    /// The id (role name) worktrees had before `id` used `--` (and, for one whose
    /// name changed, before names came from git admin dirs: `legacy`); None for the
    /// primary, whose id didn't change.
    pub fn legacy_id(&self) -> Option<String> {
        let w = self.worktree.as_deref()?;
        if let Some(old) = self.legacy() {
            return Some(old.role);
        }
        // PostgreSQL truncated long role names.
        Some(
            format!("{}-{w}", self.project)
                .chars()
                .take(PG_NAME_MAX)
                .collect(),
        )
    }

    /// Key of a one-off command's log (`setup`, `seed`) in the checkout; `+` can't
    /// meet a checkout or service id.
    pub fn run_id(&self, what: &str) -> String {
        format!("{}+{what}", self.id())
    }

    /// Key of a service's process and log.
    pub fn service_id(&self, name: &str) -> String {
        format!("{}.{name}", self.id())
    }

    /// Password of the checkout's PostgreSQL role (named `id()`).
    pub fn pg_password(&self) -> Result<String> {
        let mut h = Sha256::new();
        h.update(secret()?);
        h.update(b"postgres/");
        h.update(self.id().as_bytes());
        Ok(hex::encode(&h.finalize()[..16]))
    }

    /// Environment of the default service (the checkout's, without services).
    pub fn env(&self, g: &Global) -> Vec<(String, String)> {
        let default = self.services.default_name().map(str::to_string);
        self.service_env(g, default.as_deref())
    }

    /// Environment of a service: its port and hostname, the checkout's database and
    /// Redis (FIXME: shared by all its services for now; see `Service`), plus
    /// LAZY_COW_TREE_<SERVICE>_URL / _PORT of every service of the checkout.
    pub fn service_env(&self, g: &Global, service: Option<&str>) -> Vec<(String, String)> {
        let (host, port) = match service {
            Some(n) if self.service(n).is_some() => (self.service_host(n), self.service_port(n)),
            _ => (self.host(), self.port),
        };
        let dev = self.dev_db();
        let test = self.test_db();
        let id = self.id();
        // The user picks this checkout at lazy-cow-tree's PostgreSQL proxy.
        let pw = self.pg_password().unwrap_or_default();
        let pg_url = |db: &str| format!("postgres://{id}:{pw}@127.0.0.1:{}/{db}", g.pg_port);
        let url = |host: &str| {
            if g.https_port == 443 {
                format!("https://{host}")
            } else {
                format!("https://{host}:{}", g.https_port)
            }
        };
        let mut env: Vec<(String, String)> = vec![
            ("LAZY_COW_TREE_PROJECT".into(), self.project.clone()),
            (
                "LAZY_COW_TREE_WORKTREE".into(),
                self.worktree.clone().unwrap_or_default(),
            ),
            (
                "LAZY_COW_TREE_SERVICE".into(),
                service.unwrap_or_default().into(),
            ),
            ("LAZY_COW_TREE_HOST".into(), host.clone()),
            ("LAZY_COW_TREE_URL".into(), url(&host)),
            ("PORT".into(), port.to_string()),
            ("PGHOST".into(), "127.0.0.1".into()),
            ("PGPORT".into(), g.pg_port.to_string()),
            ("PGUSER".into(), id.clone()),
            ("PGPASSWORD".into(), pw.clone()),
            ("PGDATABASE".into(), dev.clone()),
            ("LAZY_COW_TREE_DEV_DATABASE".into(), dev.clone()),
            ("LAZY_COW_TREE_TEST_DATABASE".into(), test.clone()),
            ("DATABASE_URL".into(), pg_url(&dev)),
            ("TEST_DATABASE_URL".into(), pg_url(&test)),
            (
                "REDIS_URL".into(),
                format!("redis://:{}@127.0.0.1:{}/0", self.id(), g.redis_port),
            ),
            (
                "NODE_EXTRA_CA_CERTS".into(),
                ca_cert_path().display().to_string(),
            ),
        ];
        for kind in &self.extra_dbs {
            let var = env_var_name(kind);
            let (dev, test) = (self.dev_db_of(Some(kind)), self.test_db_of(Some(kind)));
            env.push((format!("{var}_DATABASE_URL"), pg_url(&dev)));
            env.push((format!("{var}_TEST_DATABASE_URL"), pg_url(&test)));
        }
        for (n, s) in &self.services.0 {
            let var = env_var_name(n);
            env.push((
                format!("LAZY_COW_TREE_{var}_PORT"),
                self.service_port(n).to_string(),
            ));
            if let Some(own) = &s.port_env {
                env.push((own.clone(), self.service_port(n).to_string()));
            }
            if s.http {
                env.push((
                    format!("LAZY_COW_TREE_{var}_URL"),
                    url(&self.service_host(n)),
                ));
            }
        }
        for p in self.port_slots() {
            let port = (self.port + p.offset).to_string();
            let var = format!(
                "LAZY_COW_TREE_{}_{}",
                env_var_name(&p.service),
                env_var_name(&p.name)
            );
            env.push((p.env, port.clone()));
            env.push((format!("{var}_PORT"), port));
            if p.http {
                env.push((format!("{var}_URL"), url(&self.service_host(&p.name))));
            }
        }
        let svc = service.and_then(|n| self.service(n));
        let dir = svc
            .and_then(|s| s.cwd.as_ref())
            .map_or_else(|| self.path.clone(), |d| self.path.join(d));
        env.extend(framework_env(&dir, &host));
        // The service's own env wins over everything detected.
        if let Some(s) = svc {
            env.extend(s.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        env
    }
}

/// Primary checkout of the repository containing `path`.
pub fn primary_root(path: &Path) -> Result<PathBuf> {
    let repo = Repository::discover(path)
        .with_context(|| format!("{} is not in a git repository", path.display()))?;
    let main = Repository::open(repo.commondir())?;
    match main.workdir() {
        Some(w) => Ok(w.canonicalize()?),
        None => bail!("bare repositories are not supported"),
    }
}

/// `path` (or, not there yet, its nearest directory) is in the primary checkout at
/// `root`: not in one of its worktrees, nor in another repository.
pub fn in_primary(path: &Path, root: &Path) -> bool {
    let Some(dir) = path.ancestors().find(|p| p.is_dir()) else {
        return false;
    };
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    matches!(locate(dir), Ok((r, None, _)) if r == root)
}

/// (primary root, worktree name or None, checkout path) for `path`.
pub fn locate(path: &Path) -> Result<(PathBuf, Option<String>, PathBuf)> {
    let repo = Repository::discover(path)
        .with_context(|| format!("{} is not in a git repository", path.display()))?;
    let root = primary_root(path)?;
    let top = repo
        .workdir()
        .context("bare repositories are not supported")?
        .canonicalize()?;
    if top == root {
        return Ok((root, None, top));
    }
    // A linked worktree's git dir is `<common>/worktrees/<admin name>`.
    let admin = repo.path().file_name().context("worktree git dir")?;
    Ok((root, Some(worktree_label(&admin.to_string_lossy())), top))
}

/// Which of `checkouts` a database belongs to, when more than one could own it by
/// name: the one whose dev or test database it is, else the one whose partition it is
/// with the longest test database name (`app_test_x2` is worktree `x2`'s own, not `x`'s
/// partition 2; `app_test_x22` is `x2`'s partition 2 while `x22` doesn't exist).
/// None when nobody claims it, or when checkouts tie (project `shop` worktree `dev-x`
/// and project `shop-dev` worktree `x` both name theirs `shop_dev_dev_x`): then
/// neither may open or drop it.
pub fn db_owner<'a>(
    db: &str,
    checkouts: impl IntoIterator<Item = &'a Checkout>,
) -> Option<&'a Checkout> {
    let key = |c: &Checkout| c.db_claim(db);
    let mut best: Vec<&Checkout> = Vec::new();
    for c in checkouts.into_iter().filter(|c| c.owns_db(db)) {
        match best.first().map(|b| key(b).cmp(&key(c))) {
            Some(std::cmp::Ordering::Greater) => {}
            Some(std::cmp::Ordering::Equal) => {
                if !best.iter().any(|b| b.same(c)) {
                    best.push(c);
                }
            }
            _ => best = vec![c],
        }
    }
    match best[..] {
        [one] => Some(one),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
