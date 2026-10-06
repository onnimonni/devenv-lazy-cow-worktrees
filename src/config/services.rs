//! Services of every checkout (`lazyCowTree.services`), their ports and environment
//! variable names.

use super::*;

pub(super) fn yes() -> bool {
    true
}

/// One process of every checkout (`lazy-cow-tree.services.<name>` in devenv.nix), started
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
    /// The service the shell hook (PORT) and `lazy-cow-tree service` pick without a name [default:
    /// `web`, else the first http service]. Every http service is served at
    /// <worktree>.<service>.<project>.localhost (<service>.<project>.localhost in the
    /// primary checkout).
    #[serde(default)]
    pub default: bool,
    /// Port = the checkout's base port + this (0-9) [default: position by name].
    #[serde(default)]
    pub port_offset: Option<u16>,
    /// A variable of its own holding its port (e.g. `WEB_PORT`), besides PORT and
    /// LAZY_COW_TREE_<SERVICE>_PORT, exported to every environment of the checkout.
    #[serde(default)]
    pub port_env: Option<String>,
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
    /// Files (relative to `cwd`, `*` / `?` in the file name) whose content changing
    /// restarts it if running, after the setup command when a dependency manifest or
    /// lockfile changed (`mix.lock`, `Gemfile.lock`, `package.json`, …) [default: for a command running
    /// `mix`, `mix.exs`, `mix.lock` and `config/*.exs`, which Phoenix's code reloader
    /// refuses to compile after; `[]` for others and to turn it off].
    #[serde(default)]
    pub restart_on_change: Option<Vec<String>>,
    /// Further ports it listens on (e.g. a debugger), by name, from the same 10-port
    /// block, exported to every environment of the checkout.
    #[serde(default)]
    pub ports: BTreeMap<String, ExtraPort>,
    /// When it starts: with its checkout (`up`), on its first request or as a
    /// dependency (`demand`), or only by `lazy-cow-tree service start` (`manual`).
    #[serde(default)]
    pub start: StartMode,
    /// Stop it after this many seconds without open connections through the proxy;
    /// its next request starts it again. None: never.
    #[serde(default)]
    pub idle_timeout: Option<u64>,
    /// HTTP probe its first request waits for, instead of its port listening.
    #[serde(default)]
    pub ready: Option<Ready>,
    /// Its hostname in the primary checkout instead of `<service>.<project>.localhost`;
    /// worktrees prefix theirs (`<worktree>.<hostname>`).
    #[serde(default)]
    pub hostname: Option<String>,
}

/// `Service::start`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StartMode {
    Up,
    #[default]
    Demand,
    Manual,
}

/// `Service::ready`: GET 127.0.0.1:<port><path>, ready on a 2xx or 3xx answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ready {
    #[serde(default = "root_path")]
    pub path: String,
    /// Seconds.
    #[serde(default = "sixty")]
    pub timeout: u64,
}

pub(super) fn root_path() -> String {
    "/".into()
}

pub(super) fn sixty() -> u64 {
    60
}

/// A service's secondary port (`lazy-cow-tree.services.<svc>.ports.<name>`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtraPort {
    /// Variable holding the port [default: `<NAME>_PORT`].
    #[serde(default)]
    pub env: Option<String>,
    /// Served at https://<worktree>.<name>.<project>.localhost, whose first request
    /// starts the owning service.
    #[serde(default)]
    pub http: bool,
    /// Port = the checkout's base port + this (0-9) [default: the highest free one].
    #[serde(default)]
    pub offset: Option<u16>,
}

/// A secondary port placed in a checkout's 10-port block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSlot {
    pub service: String,
    pub name: String,
    pub offset: u16,
    pub env: String,
    pub http: bool,
}

/// Every port of a checkout's block: services' offsets and their secondary ports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    pub services: BTreeMap<String, u16>,
    pub ports: Vec<PortSlot>,
}

pub(super) fn env_var_name(name: &str) -> String {
    name.to_ascii_uppercase().replace('-', "_")
}

/// Variables `Checkout::service_env` sets besides `LAZY_COW_TREE_*`; a named port's
/// `env` may not replace them.
pub const RESERVED_ENV: &[&str] = &[
    "PORT",
    "PGHOST",
    "PGPORT",
    "PGUSER",
    "PGPASSWORD",
    "PGDATABASE",
    "DATABASE_URL",
    "TEST_DATABASE_URL",
    "REDIS_URL",
    "NODE_EXTRA_CA_CERTS",
    "PHX_HOST",
    "RAILS_DEVELOPMENT_HOSTS",
    "__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS",
];

pub(super) fn valid_env_name(s: &str) -> bool {
    let mut b = s.bytes();
    b.next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == b'_')
        && b.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    #[default]
    No,
    OnFailure,
    Always,
}

/// `LAZY_COW_TREE_SERVICES`: the services as JSON (the devenv module writes it).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Services(pub BTreeMap<String, Service>);

impl std::str::FromStr for Services {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let s = if s.trim().is_empty() { "{}" } else { s };
        let services: Services =
            serde_json::from_str(s).map_err(|e| format!("LAZY_COW_TREE_SERVICES: {e}"))?;
        services.validate()?;
        Ok(services)
    }
}

impl Services {
    fn validate(&self) -> std::result::Result<(), String> {
        self.layout()?;
        for (name, s) in &self.0 {
            for d in &s.depends_on {
                if !self.0.contains_key(d) {
                    return Err(format!("service {name} depends on unknown service {d}"));
                }
            }
        }
        if self.0.values().filter(|s| s.default).count() > 1 {
            return Err("more than one default service".into());
        }
        let mut hosts = BTreeMap::new();
        for (name, s) in &self.0 {
            let Some(h) = &s.hostname else { continue };
            // Under `.localhost` or the project's tls domain: `Project::check_hostnames`.
            let valid = h == &h.to_ascii_lowercase()
                && h.contains('.')
                && h.split('.').all(|l| {
                    !l.is_empty()
                        && !l.starts_with('-')
                        && !l.ends_with('-')
                        && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                });
            if !valid {
                return Err(format!(
                    "service {name}: hostname {h} must be a lowercase DNS name"
                ));
            }
            if let Some(other) = hosts.insert(h.clone(), name.clone()) {
                return Err(format!("services {other} and {name} share hostname {h}"));
            }
        }
        Ok(())
    }

    /// Place every port in the 10-port block: services at their `portOffset` (default:
    /// position by name), then their secondary ports at their `offset`, the rest from
    /// the top down (9, 8, ...) by service and port name. Pure: the shell hook and
    /// the daemon agree.
    pub fn layout(&self) -> std::result::Result<Layout, String> {
        fn take(
            taken: &mut BTreeMap<u16, String>,
            off: u16,
            what: String,
        ) -> std::result::Result<(), String> {
            if off > 9 {
                return Err(format!("{what}: port offset {off} is over 9"));
            }
            match taken.insert(off, what.clone()) {
                Some(other) => Err(format!("{other} and {what} share port offset {off}")),
                None => Ok(()),
            }
        }
        let mut taken = BTreeMap::new();
        let mut layout = Layout::default();
        for (i, (name, s)) in self.0.iter().enumerate() {
            if !valid_label(name) {
                return Err(format!(
                    "service {name}: use a-z, 0-9 and '-' (it's a hostname)"
                ));
            }
            let off = s.port_offset.unwrap_or(i as u16);
            take(&mut taken, off, format!("service {name}"))?;
            layout.services.insert(name.clone(), off);
        }
        let mut envs: BTreeMap<String, String> = BTreeMap::new();
        // LAZY_COW_TREE_<SERVICE>_PORT / _<SERVICE>_<PORT>_PORT (and _URL) must not collide.
        let mut generated: BTreeMap<String, String> = BTreeMap::new();
        for name in self.0.keys() {
            generated.insert(env_var_name(name), format!("service {name}"));
        }
        let mut hosts: BTreeMap<&str, &str> = BTreeMap::new();
        for (svc, s) in &self.0 {
            if let Some(env) = &s.port_env {
                let what = format!("service {svc}");
                if !valid_env_name(env) {
                    return Err(format!("{what}: port env {env} is not [A-Z_][A-Z0-9_]*"));
                }
                if RESERVED_ENV.contains(&env.as_str()) || env.starts_with("LAZY_COW_TREE_") {
                    return Err(format!("{what}: port env {env} is set by lazy-cow-tree"));
                }
                if let Some(other) = envs.insert(env.clone(), what.clone()) {
                    return Err(format!("{other} and {what} share env {env}"));
                }
            }
        }
        for (svc, s) in &self.0 {
            for (name, p) in &s.ports {
                let what = format!("port {svc}.{name}");
                if !valid_label(name) {
                    return Err(format!("{what}: use a-z, 0-9 and '-' in its name"));
                }
                if p.http {
                    if self.0.contains_key(name) {
                        return Err(format!("{what}: its hostname is service {name}'s"));
                    }
                    if let Some(other) = hosts.insert(name, svc) {
                        return Err(format!("{what}: service {other} has an http port {name}"));
                    }
                }
                let env = p
                    .env
                    .clone()
                    .unwrap_or_else(|| format!("{}_PORT", env_var_name(name)));
                if !valid_env_name(&env) {
                    return Err(format!("{what}: env {env} is not [A-Z_][A-Z0-9_]*"));
                }
                if RESERVED_ENV.contains(&env.as_str()) || env.starts_with("LAZY_COW_TREE_") {
                    return Err(format!("{what}: env {env} is set by lazy-cow-tree"));
                }
                if let Some(other) = envs.insert(env.clone(), what.clone()) {
                    return Err(format!("{other} and {what} share env {env}"));
                }
                let var = format!("{}_{}", env_var_name(svc), env_var_name(name));
                if let Some(other) = generated.insert(var.clone(), what.clone()) {
                    return Err(format!(
                        "{other} and {what} both set LAZY_COW_TREE_{var}_PORT"
                    ));
                }
                if let Some(off) = p.offset {
                    take(&mut taken, off, what)?;
                }
                layout.ports.push(PortSlot {
                    service: svc.clone(),
                    name: name.clone(),
                    offset: p.offset.unwrap_or(u16::MAX),
                    env,
                    http: p.http,
                });
            }
        }
        for slot in layout.ports.iter_mut().filter(|p| p.offset == u16::MAX) {
            let what = format!("port {}.{}", slot.service, slot.name);
            let off = (0..=9u16)
                .rev()
                .find(|o| !taken.contains_key(o))
                .ok_or_else(|| {
                    format!("{what}: no free port offset (the 10-port block is full)")
                })?;
            take(&mut taken, off, what)?;
            slot.offset = off;
        }
        Ok(layout)
    }

    pub fn offset(&self, name: &str) -> u16 {
        self.layout()
            .ok()
            .and_then(|l| l.services.get(name).copied())
            .unwrap_or(0)
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
