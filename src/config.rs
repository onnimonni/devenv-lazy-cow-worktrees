//! Paths, settings and the naming scheme every part of the service shares. Names and
//! ports are derived from a checkout's path alone, so `localforest env` in a worktree
//! computes the same values as the daemon without asking it.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use git2::Repository;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    /// PostgreSQL port on 127.0.0.1 (localforest's proxy; the user picks the checkout).
    /// The real server listens on <state>/pg/.s.PGSQL.<port + 1> only.
    #[arg(
        long,
        env = "LOCALFOREST_PG_PORT",
        default_value_t = 55432,
        global = true
    )]
    pub pg_port: u16,
    /// Redis port on 127.0.0.1. The password picks the checkout: each has its own
    /// redis-server behind it (REDIS_URL in `localforest env`).
    #[arg(
        long,
        env = "LOCALFOREST_REDIS_PORT",
        default_value_t = 6380,
        global = true
    )]
    pub redis_port: u16,
    /// HTTPS proxy port [default: 443 where unprivileged processes may bind it (macOS;
    /// Linux with net.ipv4.ip_unprivileged_port_start <= 443), else 8443]. Below 1024
    /// it binds all interfaces and refuses non-loopback peers.
    #[arg(
        long,
        env = "LOCALFOREST_HTTPS_PORT",
        default_value_t = default_https_port(),
        global = true
    )]
    pub https_port: u16,
    /// Plain HTTP port that redirects to HTTPS; 0 disables [default: 80 where
    /// unprivileged processes may bind it, else off].
    #[arg(
        long,
        env = "LOCALFOREST_HTTP_PORT",
        default_value_t = default_http_port(),
        global = true
    )]
    pub http_port: u16,
    /// PostgreSQL RAM disk size in MB (memory is only used as it fills).
    #[arg(
        long,
        env = "LOCALFOREST_RAMDISK_MB",
        default_value_t = 4096,
        global = true
    )]
    pub ramdisk_mb: u64,
    /// Directory with PostgreSQL's `postgres` and `initdb` [default: from PATH].
    #[arg(long, env = "LOCALFOREST_POSTGRES_BIN", global = true)]
    pub postgres_bin: Option<PathBuf>,
    /// Extra postgresql.conf settings as a JSON object, e.g.
    /// {"shared_preload_libraries": "pg_stat_statements"}.
    #[arg(long, env = "LOCALFOREST_POSTGRES_SETTINGS", global = true)]
    pub postgres_settings: Option<String>,
    /// The `redis-server` to run [default: from PATH].
    #[arg(long, env = "LOCALFOREST_REDIS_SERVER", global = true)]
    pub redis_server: Option<PathBuf>,
}

impl Global {
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
            serde_json::from_str(json).context("LOCALFOREST_POSTGRES_SETTINGS")?;
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

/// Per-project settings, sent by `localforest serve` when it registers a project.
#[derive(clap::Args, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectSettings {
    /// Project name: <name>.localhost, <worktree>.<name>.localhost, database prefix
    /// [default: primary checkout's directory name].
    #[arg(long = "project", env = "LOCALFOREST_PROJECT")]
    pub name: Option<String>,
    /// Port the primary checkout's app listens on (worktrees get their own).
    #[arg(long, env = "LOCALFOREST_PORT", default_value_t = 4000)]
    pub port: u16,
    /// Git remote to fetch from and watch on GitHub.
    #[arg(long, env = "LOCALFOREST_REMOTE", default_value = "origin")]
    pub remote: String,
    /// Base branch [default: remote HEAD, else main].
    #[arg(long, env = "LOCALFOREST_BASE")]
    pub base: Option<String>,
    /// Where new worktrees go, relative to the primary checkout.
    #[arg(
        long,
        env = "LOCALFOREST_WORKTREES_DIR",
        default_value = ".claude/worktrees"
    )]
    pub worktrees_dir: PathBuf,
    /// Migrate/seed command, run with the checkout's env: in the primary checkout when
    /// the base branch moves (then the template database is refreshed from it), and in
    /// every worktree the base branch was merged into. Split like a shell would, but
    /// run directly, e.g. "mix do ecto.migrate + run priv/repo/seeds.exs".
    #[arg(long, env = "LOCALFOREST_MIGRATE")]
    pub migrate: Option<String>,
    /// Seed command, run in the primary checkout after `migrate` when its database
    /// was just created (empty); worktrees get the seeded data through the template.
    /// E.g. "mix run priv/repo/seeds.exs".
    #[arg(long, env = "LOCALFOREST_SEED")]
    pub seed: Option<String>,
    /// Setup command, run once in every new checkout (made by localforest, git,
    /// git-cow or Claude Code) with its env, before its services start, e.g.
    /// "mix deps.get". Checkouts it ran in are marked in their git admin dir.
    #[arg(long, env = "LOCALFOREST_SETUP")]
    pub setup: Option<String>,
    /// Services of every checkout as JSON ({"web": {"exec": "mix phx.server"}, ...};
    /// see `Service`), written by the devenv module's `localforest.services`. Each is
    /// started on demand (its first request, or as a dependency) with its own port and
    /// hostname, sharing the checkout's database and Redis.
    #[arg(long, env = "LOCALFOREST_SERVICES", default_value = "{}")]
    pub services: Services,
    /// Close a preview (a removed worktree recreated from its "gone" page) after this
    /// many hours without requests or database / Redis connections; 0 keeps them.
    #[arg(long, env = "LOCALFOREST_PREVIEW_TTL_HOURS", default_value_t = 48)]
    pub preview_ttl_hours: u64,
    /// Don't pull branches / merge the base branch into worktrees on pushes.
    #[arg(long, env = "LOCALFOREST_NO_SYNC", value_parser = clap::builder::BoolishValueParser::new())]
    pub no_sync: bool,
    /// Don't remove worktrees whose PR merged.
    #[arg(long, env = "LOCALFOREST_NO_AUTO_REMOVE", value_parser = clap::builder::BoolishValueParser::new())]
    pub no_auto_remove: bool,
}

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("LOCALFOREST_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    home.join(".local/state/localforest")
}

pub fn socket_path() -> PathBuf {
    home().join("localforest.sock")
}

/// RAM disk mount point; also PostgreSQL's unix socket directory.
pub fn pg_dir() -> PathBuf {
    home().join("pg")
}

pub fn ca_cert_path() -> PathBuf {
    home().join("ca/ca.pem")
}

/// Per-machine random secret that checkout passwords derive from, created on first
/// use (0600), so `localforest env` and the daemon agree without talking.
pub fn secret() -> Result<Vec<u8>> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = home().join("secret");
    if let Ok(s) = std::fs::read(&path)
        && s.len() >= 32
    {
        return Ok(s);
    }
    std::fs::create_dir_all(home())?;
    let mut s = vec![0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut s))
        .context("reading /dev/urandom")?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(&s)?;
            Ok(s)
        }
        // Another process won the race.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(std::fs::read(&path)?),
        Err(e) => Err(e.into()),
    }
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
    /// Environment of the `localforest serve` that registered it (PATH for the migrate
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

    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join(&self.settings.worktrees_dir)
    }

    pub fn checkout(&self, worktree: Option<&str>, path: PathBuf) -> Checkout {
        self.checkout_avoiding(worktree, path, &Default::default())
    }

    /// `checkout`, a new worktree's port also avoiding `taken` (see `worktree::port`).
    pub fn checkout_avoiding(
        &self,
        worktree: Option<&str>,
        path: PathBuf,
        taken: &std::collections::HashSet<u16>,
    ) -> Checkout {
        let port = match worktree {
            None => self.settings.port,
            Some(w) if path.as_os_str().is_empty() => worktree_port(&self.name, w),
            Some(w) => crate::worktree::port(self, &path, w, taken).unwrap_or_else(|e| {
                tracing::warn!("worktree {w}: port: {e:#}");
                worktree_port(&self.name, w)
            }),
        };
        Checkout {
            project: self.name.clone(),
            db_prefix: self.db_prefix(),
            worktree: worktree.map(str::to_string),
            path,
            port,
            services: self.settings.services.clone(),
        }
    }
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

fn yes() -> bool {
    true
}

/// One process of every checkout (`localforest.services.<name>` in devenv.nix), started
/// on demand with the checkout's environment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Service {
    /// Command, split like a shell would and run directly.
    pub exec: String,
    /// Working directory relative to the checkout.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Listens on $PORT and gets a hostname; started by its first request. Services
    /// without http run only as dependencies of others (workers).
    #[serde(default = "yes")]
    pub http: bool,
    /// The service `localforest env` and `localforest service` pick without a name [default:
    /// `web`, else the first http service]. Every http service is served at
    /// <worktree>.<service>.<project>.localhost (<service>.<project>.localhost in the
    /// primary checkout).
    #[serde(default)]
    pub default: bool,
    /// Port = the checkout's base port + this (0-9) [default: position by name].
    #[serde(default)]
    pub port_offset: Option<u16>,
    // FIXME: every service of a checkout shares its one DATABASE_URL and REDIS_URL.
    // Support services with databases / redis-servers of their own (e.g. `postgres` /
    // `redis` flags: <project>_<service>_dev_<worktree> cloned from
    // <project>_<service>_template, a redis-server per service behind the Redis port).
    /// Migrate/seed command for the checkout's database, run in its `cwd` after the
    /// project's.
    #[serde(default)]
    pub migrate: Option<String>,
    /// Services started before this one.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Extra environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// When it exits on its own: leave it (`no`), start it again after a failure
    /// (`on-failure`) or after any exit (`always`), backing off up to 30 s.
    #[serde(default)]
    pub restart: Restart,
    /// Restart it (if running) after the base branch was pulled into its checkout and
    /// the migrations ran, for servers without a code reloader.
    #[serde(default)]
    pub restart_on_pull: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    #[default]
    No,
    OnFailure,
    Always,
}

/// `LOCALFOREST_SERVICES`: the services as JSON (the devenv module writes it).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Services(pub BTreeMap<String, Service>);

impl std::str::FromStr for Services {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let s = if s.trim().is_empty() { "{}" } else { s };
        let services: Services =
            serde_json::from_str(s).map_err(|e| format!("LOCALFOREST_SERVICES: {e}"))?;
        services.validate()?;
        Ok(services)
    }
}

impl Services {
    fn validate(&self) -> std::result::Result<(), String> {
        let mut seen = BTreeMap::new();
        for (name, s) in &self.0 {
            if !valid_label(name) {
                return Err(format!(
                    "service {name}: use a-z, 0-9 and '-' (it's a hostname)"
                ));
            }
            let off = self.offset(name);
            if off > 9 {
                return Err(format!("service {name}: port offset {off} is over 9"));
            }
            if let Some(other) = seen.insert(off, name) {
                return Err(format!(
                    "services {other} and {name} share port offset {off}"
                ));
            }
            for d in &s.depends_on {
                if !self.0.contains_key(d) {
                    return Err(format!("service {name} depends on unknown service {d}"));
                }
            }
        }
        if self.0.values().filter(|s| s.default).count() > 1 {
            return Err("more than one default service".into());
        }
        Ok(())
    }

    pub fn offset(&self, name: &str) -> u16 {
        let s = &self.0[name];
        s.port_offset
            .unwrap_or_else(|| self.0.keys().position(|k| k == name).unwrap_or(0) as u16)
    }

    /// The service served at the checkout's own hostname.
    pub fn default_name(&self) -> Option<&str> {
        self.0
            .iter()
            .find(|(_, s)| s.default)
            .or_else(|| self.0.get_key_value("web").filter(|(_, s)| s.http))
            .or_else(|| self.0.iter().find(|(_, s)| s.http))
            .map(|(k, _)| k.as_str())
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
}

impl Checkout {
    pub fn host(&self) -> String {
        match &self.worktree {
            Some(w) => format!("{w}.{}.localhost", self.project),
            None => format!("{}.localhost", self.project),
        }
    }

    fn suffix(&self) -> String {
        self.worktree
            .as_deref()
            .map(|w| format!("_{}", w.replace('-', "_")))
            .unwrap_or_default()
    }

    pub fn service(&self, name: &str) -> Option<&Service> {
        self.services.0.get(name)
    }

    pub fn service_port(&self, name: &str) -> u16 {
        self.port + self.services.offset(name)
    }

    /// `<worktree>.<service>.<project>.localhost`, `<service>.<project>.localhost` in
    /// the primary checkout.
    pub fn service_host(&self, name: &str) -> String {
        match &self.worktree {
            Some(w) => format!("{w}.{name}.{}.localhost", self.project),
            None => format!("{name}.{}.localhost", self.project),
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
            .collect()
    }

    /// Host of the default service (the checkout's own without services).
    pub fn main_host(&self) -> String {
        match self.services.default_name() {
            Some(d) => self.service_host(d),
            None => self.host(),
        }
    }

    pub fn dev_db(&self) -> String {
        format!("{}_dev{}", self.db_prefix, self.suffix())
    }

    pub fn test_db(&self) -> String {
        format!("{}_test{}", self.db_prefix, self.suffix())
    }

    /// The project's template database, which worktrees' dev databases are cloned from.
    pub fn template_db(&self) -> String {
        format!("{}_template", self.db_prefix)
    }

    /// Databases this checkout owns: dev, test and MIX_TEST_PARTITION style
    /// `<prefix>_test<N>_<wt>` / `<prefix>_test_p<N>_<wt>`.
    pub fn owns_db(&self, db: &str) -> bool {
        if db == self.dev_db() || db == self.test_db() {
            return true;
        }
        let Some(rest) = db.strip_prefix(&format!("{}_test", self.db_prefix)) else {
            return false;
        };
        let Some(mid) = rest.strip_suffix(&self.suffix()) else {
            return false;
        };
        let mid = mid.trim_start_matches('_').trim_start_matches('p');
        !mid.is_empty() && mid.bytes().all(|b| b.is_ascii_digit())
    }

    /// Key for per-checkout processes and logs; also its PostgreSQL role and the
    /// Redis password that picks its redis-server behind the shared Redis port.
    pub fn id(&self) -> String {
        match &self.worktree {
            Some(w) => format!("{}-{w}", self.project),
            None => self.project.clone(),
        }
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
    /// LOCALFOREST_<SERVICE>_URL / _PORT of every service of the checkout.
    pub fn service_env(&self, g: &Global, service: Option<&str>) -> Vec<(String, String)> {
        let (host, port) = match service {
            Some(n) if self.service(n).is_some() => (self.service_host(n), self.service_port(n)),
            _ => (self.host(), self.port),
        };
        let dev = self.dev_db();
        let test = self.test_db();
        let id = self.id();
        // The user picks this checkout at localforest's PostgreSQL proxy.
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
            ("LOCALFOREST_PROJECT".into(), self.project.clone()),
            (
                "LOCALFOREST_WORKTREE".into(),
                self.worktree.clone().unwrap_or_default(),
            ),
            (
                "LOCALFOREST_SERVICE".into(),
                service.unwrap_or_default().into(),
            ),
            ("LOCALFOREST_HOST".into(), host.clone()),
            ("LOCALFOREST_URL".into(), url(&host)),
            ("PORT".into(), port.to_string()),
            ("PGHOST".into(), "127.0.0.1".into()),
            ("PGPORT".into(), g.pg_port.to_string()),
            ("PGUSER".into(), id.clone()),
            ("PGPASSWORD".into(), pw.clone()),
            ("PGDATABASE".into(), dev.clone()),
            ("LOCALFOREST_DEV_DATABASE".into(), dev.clone()),
            ("LOCALFOREST_TEST_DATABASE".into(), test.clone()),
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
        for (n, s) in &self.services.0 {
            let var = n.to_ascii_uppercase().replace('-', "_");
            env.push((
                format!("LOCALFOREST_{var}_PORT"),
                self.service_port(n).to_string(),
            ));
            if s.http {
                env.push((format!("LOCALFOREST_{var}_URL"), url(&self.service_host(n))));
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

/// Variables that make the framework of the app in `dir` accept its
/// https://…localhost hostname, detected from its manifests:
/// Phoenix (`PHX_HOST`), Rails (`RAILS_DEVELOPMENT_HOSTS`, host authorization) and
/// Vite (`__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS`, allowed-hosts check).
pub fn framework_env(dir: &Path, host: &str) -> Vec<(String, String)> {
    let read = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let mut env = Vec::new();
    // Umbrella projects declare Phoenix in apps/*/mix.exs.
    let mut mix = read(dir.join("mix.exs"));
    if let Ok(apps) = std::fs::read_dir(dir.join("apps")) {
        for app in apps.flatten() {
            mix += &read(app.path().join("mix.exs"));
        }
    }
    if mix.contains("{:phoenix,") {
        env.push(("PHX_HOST".into(), host.into()));
    }
    let gemfile = read(dir.join("Gemfile"));
    if gemfile.contains("gem \"rails\"") || gemfile.contains("gem 'rails'") {
        env.push(("RAILS_DEVELOPMENT_HOSTS".into(), host.into()));
    }
    if read(dir.join("package.json")).contains("\"vite\"") {
        env.push(("__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS".into(), host.into()));
    }
    env
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
    let name = dns_label(&top.file_name().unwrap_or_default().to_string_lossy());
    Ok((root, Some(name), top))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn co(wt: Option<&str>) -> Checkout {
        Checkout {
            project: "my-app".into(),
            db_prefix: "my_app".into(),
            worktree: wt.map(str::to_string),
            path: "/x".into(),
            port: 20000,
            services: Services::default(),
        }
    }

    fn with_services(json: &str) -> Checkout {
        Checkout {
            services: json.parse().unwrap(),
            ..co(Some("wt"))
        }
    }

    #[test]
    fn services() {
        let c = with_services(
            r#"{"web": {"exec": "mix phx.server", "dependsOn": ["worker"]},
                "api": {"exec": "bun dev", "cwd": "api"},
                "worker": {"exec": "mix run", "http": false}}"#,
        );
        assert_eq!(c.services.default_name(), Some("web"));
        // Offsets by name: api 0, web 1, worker 2.
        assert_eq!(c.service_port("api"), 20000);
        assert_eq!(c.service_port("web"), 20001);
        assert_eq!(c.service_host("web"), "wt.web.my-app.localhost");
        assert_eq!(c.service_host("api"), "wt.api.my-app.localhost");
        assert_eq!(
            c.routes(),
            vec![
                (Some("api".into()), "wt.api.my-app.localhost".into(), 20000),
                (Some("web".into()), "wt.web.my-app.localhost".into(), 20001),
            ]
        );
        assert_eq!(c.main_host(), "wt.web.my-app.localhost");
        let primary = Checkout {
            worktree: None,
            ..c.clone()
        };
        assert_eq!(primary.service_host("api"), "api.my-app.localhost");

        // Every service shares the checkout's database and Redis (for now).
        let env: BTreeMap<_, _> = c.service_env(&global(), Some("api")).into_iter().collect();
        assert_eq!(env["PORT"], "20000");
        assert_eq!(env["LOCALFOREST_URL"], "https://wt.api.my-app.localhost");
        assert_eq!(env["PGDATABASE"], "my_app_dev_wt");
        assert!(env["REDIS_URL"].starts_with("redis://:my-app-wt@"));
        assert_eq!(
            env["LOCALFOREST_WEB_URL"],
            "https://wt.web.my-app.localhost"
        );
        assert_eq!(env["LOCALFOREST_WORKER_PORT"], "20002");
        assert!(!env.contains_key("LOCALFOREST_WORKER_URL"));
        let web: BTreeMap<_, _> = c.env(&global()).into_iter().collect();
        assert_eq!(web["PORT"], "20001");
        assert_eq!(web["DATABASE_URL"], env["DATABASE_URL"]);
        assert_eq!(web["REDIS_URL"], env["REDIS_URL"]);

        assert!(
            "{\"a\": {\"exec\": \"x\", \"dependsOn\": [\"b\"]}}"
                .parse::<Services>()
                .is_err()
        );
        assert!(
            "{\"a\": {\"exec\": \"x\", \"portOffset\": 10}}"
                .parse::<Services>()
                .is_err()
        );
        assert!(
            "{\"a\": {\"exec\": \"x\", \"bogus\": 1}}"
                .parse::<Services>()
                .is_err()
        );
    }

    #[test]
    fn detects_frameworks() {
        let dir = tempfile::TempDir::new().unwrap();
        let d = dir.path();
        let keys = |d: &Path| -> Vec<String> {
            framework_env(d, "h.localhost")
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        assert!(keys(d).is_empty());
        // Umbrella: Phoenix only in an app.
        std::fs::create_dir_all(d.join("apps/web")).unwrap();
        std::fs::write(d.join("mix.exs"), "defmodule U.MixProject do end").unwrap();
        std::fs::write(
            d.join("apps/web/mix.exs"),
            r#"defp deps, do: [{:phoenix, "~> 1.8"}]"#,
        )
        .unwrap();
        assert_eq!(keys(d), ["PHX_HOST"]);
        std::fs::write(d.join("Gemfile"), "gem \"rails\", \"~> 8.0\"").unwrap();
        std::fs::write(
            d.join("package.json"),
            r#"{"devDependencies": {"vite": "^7"}}"#,
        )
        .unwrap();
        assert_eq!(
            keys(d),
            [
                "PHX_HOST",
                "RAILS_DEVELOPMENT_HOSTS",
                "__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS"
            ]
        );
        // phoenix_live_view alone isn't Phoenix.
        let other = tempfile::TempDir::new().unwrap();
        std::fs::write(
            other.path().join("mix.exs"),
            r#"[{:phoenix_live_view, "~> 1.0"}]"#,
        )
        .unwrap();
        assert!(keys(other.path()).is_empty());
    }

    #[test]
    fn postgres_settings() {
        assert_eq!(
            global().postgres_settings().unwrap(),
            vec![
                ("jit".to_string(), "off".to_string()),
                ("n".into(), "3".into()),
                ("shared_preload_libraries".into(), "x".into()),
            ]
        );
    }

    fn global() -> Global {
        Global {
            pg_port: 55432,
            redis_port: 6380,
            https_port: 443,
            http_port: 80,
            ramdisk_mb: 1,
            postgres_bin: None,
            postgres_settings: Some(
                r#"{"shared_preload_libraries": "x", "jit": false, "n": 3}"#.into(),
            ),
            redis_server: None,
        }
    }

    #[test]
    fn names() {
        assert_eq!(dns_label("Fix Login_Bug!"), "fix-login-bug");
        assert_eq!(co(None).host(), "my-app.localhost");
        assert_eq!(co(Some("fix-it")).host(), "fix-it.my-app.localhost");
        assert_eq!(co(Some("fix-it")).dev_db(), "my_app_dev_fix_it");
        assert_eq!(co(None).test_db(), "my_app_test");
        let p = worktree_port("a", "b");
        assert!((20000..29000).contains(&p) && p.is_multiple_of(10));
    }

    #[test]
    fn owned_databases() {
        let c = co(Some("wt"));
        assert!(c.owns_db("my_app_dev_wt"));
        assert!(c.owns_db("my_app_test_wt"));
        assert!(c.owns_db("my_app_test2_wt"));
        assert!(c.owns_db("my_app_test_p3_wt"));
        assert!(!c.owns_db("my_app_test_other_wt"));
        assert!(!c.owns_db("my_app_dev"));
        assert!(!co(None).owns_db("my_app_dev_wt"));
    }
}
