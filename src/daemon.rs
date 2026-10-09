//! The daemon: one per user, shared by every project that registers with it.
//!
//! - PostgreSQL on an APFS RAM disk; each worktree gets `<project>_dev_<worktree>`, a
//!   copy-on-write clone of `<project>_template`
//! - services per checkout at https://<worktree>.<service>.<project>.localhost; the
//!   first request starts one (after what it depends on) if nothing listens
//! - one Redis port; the password picks the checkout's own redis-server
//! - reconciles `git worktree list` on start, every minute and when asked (the devenv
//!   module's `git` wrapper, Claude Code's hooks): new worktrees are provisioned,
//!   deleted ones cleaned up
//! - GitHub webhook websocket (polling as fallback): pushes pull branches and merge
//!   the base branch into worktrees, which are then migrated (new worktrees are
//!   migrated once too); when the base branch moves, the migrate command runs in the
//!   primary checkout and the template is refreshed from its database; worktrees whose PR merged are removed (their
//!   processes killed)
//!
//! `lazy-cow-tree serve` in any project either becomes the daemon or registers its project
//! with the running one, then takes over if that one goes away.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use git2::{Oid, Repository};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::{net::UnixListener, sync::Notify, task::JoinHandle};
use tracing::{debug, error, info, warn};

use crate::{
    config::{self, Checkout, Global, Project, StartMode},
    github,
    postgres::Postgres,
    proxy::{self, Routes},
    redis::Redis,
    server::Servers,
    sync::Syncer,
    tls::{self, Ca, trusted::Trusted},
    worktree::{self, Safety},
};

mod api;
mod databases;
mod migrate;
mod services;
mod tasks;
mod worktrees;

use self::api::{api, dashboard};
use self::tasks::{domain_certificate, github_client, ticker, watch_github, worker};

pub struct Daemon {
    global: Global,
    pg: Postgres,
    /// Serialises on-demand CREATE DATABASE and swapping in a new template.
    create_lock: tokio::sync::Mutex<()>,
    /// Per dev database: its first connects wait until it's cloned and adopted.
    dev_locks: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    redis: Arc<Redis>,
    servers: Arc<Servers>,
    routes: Routes,
    /// Projects' domain certificates, served by the proxy.
    trusted: Arc<Trusted>,
    projects: Mutex<BTreeMap<PathBuf, Arc<ProjectRt>>>,
    shutdown: Notify,
    /// Checkouts (by project root) whose `start = "up"` services to start, drained
    /// every second.
    up_queue: Mutex<Vec<(PathBuf, Checkout)>>,
}

#[derive(Default)]
struct Pending {
    reconcile: AtomicBool,
    sync: AtomicBool,
    merged: AtomicBool,
    migrate: AtomicBool,
    /// Worktrees whose database has not been migrated yet (`migrate_worktrees`).
    migrate_worktrees: AtomicBool,
}

/// A checkout's last failed migration: shown in `status` and on its 502 page, and
/// retried (by the sweep) only after a backoff that doubles with every attempt.
#[derive(Clone)]
struct MigrateFailure {
    error: String,
    attempts: u32,
    at: std::time::Instant,
}

impl MigrateFailure {
    fn backing_off(&self) -> bool {
        let backoff = Duration::from_secs(60 << self.attempts.min(6).saturating_sub(1));
        self.at.elapsed() < backoff
    }
}

/// Marker of a checkout's succeeded setup, in its git admin dir (a worktree's goes
/// away with it).
fn setup_marker(path: &Path) -> Result<PathBuf> {
    Ok(Repository::open(path)?.path().join(worktree::SETUP_MARKER))
}

#[derive(Debug, PartialEq)]
enum SetupStep {
    Done,
    Run,
    BackingOff,
}

/// Whether the primary's setup runs now: when pending, unless its last failure
/// (a failed setup, since its marker is missing) is still backing off and not `force`.
fn primary_setup_step(pending: bool, failure: Option<&MigrateFailure>, force: bool) -> SetupStep {
    match failure {
        _ if !pending => SetupStep::Done,
        Some(f) if !force && f.backing_off() => SetupStep::BackingOff,
        _ => SetupStep::Run,
    }
}

struct ProjectRt {
    project: Project,
    base: String,
    /// Serialises everything that changes worktrees or databases.
    lock: tokio::sync::Mutex<()>,
    /// Provisioned worktrees by name.
    known: Mutex<BTreeMap<String, Checkout>>,
    gh: Option<Arc<github::Client>>,
    token: Option<String>,
    pending: Pending,
    wake: Notify,
    last_migrated: Mutex<Option<Oid>>,
    /// The primary's dev database was just created: seed it after migrating.
    seed_pending: AtomicBool,
    /// Per checkout id: held while its migrations run, so its services wait for them
    /// without holding up `lock`.
    migrating: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    migrate_failures: Mutex<BTreeMap<String, MigrateFailure>>,
    ws_ok: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The primary's `start = "up"` services were queued.
    up_queued: AtomicBool,
}

impl ProjectRt {
    fn syncer(&self) -> Syncer {
        Syncer {
            path: self.project.root.clone(),
            remote: self.project.settings.remote.clone(),
            base: self.base.clone(),
            token: self.token.clone(),
            token_host: self
                .gh
                .as_ref()
                .map_or_else(|| "github.com".into(), |g| g.repo.host.clone()),
        }
    }

    fn primary(&self) -> Checkout {
        self.project.checkout(None, self.project.root.clone())
    }

    fn migrate_lock(&self, c: &Checkout) -> Arc<tokio::sync::Mutex<()>> {
        self.migrating.lock().entry(c.id()).or_default().clone()
    }

    fn has_migrations(&self, c: &Checkout) -> bool {
        self.project.settings.migrate.is_some()
            || c.services.0.values().any(|s| s.migrate.is_some())
    }

    fn migrate_failure(&self, c: &Checkout) -> Option<MigrateFailure> {
        self.migrate_failures.lock().get(&c.id()).cloned()
    }

    /// Record how migrating `c` went: None when it succeeded.
    fn migrated(&self, c: &Checkout, error: Option<String>) {
        let mut failures = self.migrate_failures.lock();
        match error {
            None => {
                failures.remove(&c.id());
            }
            Some(error) => {
                let attempts = failures.get(&c.id()).map_or(0, |f| f.attempts) + 1;
                failures.insert(
                    c.id(),
                    MigrateFailure {
                        error,
                        attempts,
                        at: std::time::Instant::now(),
                    },
                );
            }
        }
    }

    /// A setup command is configured and hasn't succeeded in `c` yet.
    fn setup_pending(&self, c: &Checkout) -> bool {
        self.project.settings.setup.is_some() && setup_marker(&c.path).is_ok_and(|m| !m.exists())
    }

    fn trigger(&self, flag: &AtomicBool) {
        flag.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
}

impl Drop for ProjectRt {
    fn drop(&mut self) {
        for t in self.tasks.lock().drain(..) {
            t.abort();
        }
    }
}

// ---------- persistence

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    projects: Vec<Project>,
}

fn state_file() -> PathBuf {
    config::home().join("state.json")
}

/// Written for its owner only (0600, also when it existed): the registered projects'
/// environments hold their secrets, GH_TOKEN included.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt, os::unix::fs::PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(data)?;
    Ok(())
}

fn load_saved() -> Saved {
    std::fs::read(state_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Projects the daemon registered (its saved state), for tools that run without it.
pub fn registered_projects() -> Vec<Project> {
    load_saved().projects
}

/// Checkout of worktree `i` on its port from `worktree::assign_ports` / `plan_ports`.
pub fn checkout_with(
    project: &Project,
    i: &worktree::Info,
    ports: &HashMap<PathBuf, u16>,
) -> Checkout {
    match ports.get(&i.path) {
        Some(&port) => project.checkout_on(Some(&i.name), i.path.clone(), port),
        None => project.checkout(Some(&i.name), i.path.clone()),
    }
}

// ---------- status types shared with the CLI

#[derive(Serialize, Deserialize, Debug)]
pub struct CheckoutStatus {
    pub checkout: Checkout,
    pub url: String,
    pub databases: Vec<String>,
    /// Their size together.
    #[serde(default)]
    pub database_bytes: u64,
    pub services: Vec<ServiceStatus>,
    /// Its redis-server is running.
    pub redis: bool,
    pub branch: Option<String>,
    /// Its last migration (or the primary's setup) failed (retried with a backoff).
    #[serde(default)]
    pub migrate_error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ServiceStatus {
    pub name: String,
    pub url: Option<String>,
    pub port: u16,
    /// Started by lazy-cow-tree and running.
    pub running: bool,
    /// Down since `lazy-cow-tree service stop`: nothing starts it but `service start`.
    #[serde(default)]
    pub stopped: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ProjectStatus {
    pub project: Project,
    pub base: String,
    pub github: Option<String>,
    pub websocket: bool,
    pub checkouts: Vec<CheckoutStatus>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Status {
    pub pid: u32,
    /// Its lazy-cow-tree version (empty from daemons before 0.4.1).
    #[serde(default)]
    pub version: String,
    pub projects: Vec<ProjectStatus>,
    pub pg_port: u16,
    pub redis_port: u16,
    pub https_port: u16,
    /// (free, total) bytes on the databases' disk (the RAM disk).
    #[serde(default)]
    pub pg_disk: Option<(u64, u64)>,
}

#[derive(Serialize, Deserialize)]
pub struct CreateReq {
    pub root: PathBuf,
    pub name: String,
    pub base: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct CreateResp {
    pub path: PathBuf,
    pub url: String,
    pub env: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
pub struct RemoveReq {
    pub root: PathBuf,
    pub name: String,
    pub force: bool,
    /// Processes not to stop, with all their ancestors (the caller, its shell, the
    /// Claude Code session that ran it).
    #[serde(default)]
    pub keep_pids: Vec<i32>,
}

#[derive(Serialize, Deserialize)]
pub struct RootReq {
    pub root: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct ServiceReq {
    pub root: PathBuf,
    /// None: the primary checkout.
    pub worktree: Option<String>,
    /// None: the default service.
    pub service: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ServiceEnvReq {
    pub service: ServiceReq,
    /// KEY=VALUE.
    pub set: Vec<String>,
    pub unset: Vec<String>,
}

// ---------- daemon

impl Daemon {
    fn project(&self, root: &Path) -> Result<Arc<ProjectRt>> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        self.projects.lock().get(&root).cloned().ok_or_else(|| {
            anyhow!(
                "{} is not registered; run `lazy-cow-tree serve` in it",
                root.display()
            )
        })
    }

    fn save(&self) {
        let saved = Saved {
            projects: self
                .projects
                .lock()
                .values()
                .map(|p| p.project.clone())
                .collect(),
        };
        if let Err(e) = serde_json::to_vec_pretty(&saved)
            .map_err(anyhow::Error::from)
            .and_then(|b| write_private(&state_file(), &b))
        {
            warn!("saving state: {e:#}");
        }
    }

    pub async fn register(self: &Arc<Self>, project: Project) -> Result<()> {
        let root = project.root.clone();
        if let Some(existing) = self.projects.lock().get(&root)
            && existing.project == project
        {
            return Ok(());
        }
        Repository::open(&root)
            .with_context(|| format!("{} is not a git checkout", root.display()))?;
        project.check_hostnames().map_err(anyhow::Error::msg)?;
        durability_check(
            self.global.postgres_durable,
            &project,
            self.projects.lock().values().map(|rt| &rt.project),
        )?;
        if let Some(other) = self
            .projects
            .lock()
            .values()
            .find(|p| p.project.name == project.name && p.project.root != root)
        {
            anyhow::bail!(
                "project name {} is taken by {}; set lazy-cow-tree.project (LAZY_COW_TREE_PROJECT) to another name",
                project.name,
                other.project.root.display()
            );
        }
        // A checkout already registered may be running its services on its ports.
        let fresh = !self.projects.lock().contains_key(&root);
        let others: Vec<Project> = self
            .projects
            .lock()
            .values()
            .filter(|p| p.project.root != root)
            .map(|p| p.project.clone())
            .collect();
        let (p, strict) = (project.clone(), config::strict_ports(&root));
        tokio::task::spawn_blocking(move || {
            worktree::assign_primary_port(&p, &others, fresh, strict)
        })
        .await??;
        let base = project
            .settings
            .base
            .clone()
            .unwrap_or_else(|| Syncer::default_base(&root, &project.settings.remote));

        let (gh, token) = match github_client(&project) {
            Ok((c, t)) => (Some(Arc::new(c)), Some(t)),
            Err(e) => {
                warn!("{}: no GitHub integration: {e:#}", project.name);
                (None, None)
            }
        };
        // An earlier registration's worktrees stay connectable (the PostgreSQL proxy
        // refuses unknown roles) until reconcile has provisioned them again.
        let known = self
            .projects
            .lock()
            .get(&root)
            .map(|old| old.known.lock().clone())
            .unwrap_or_default();
        let rt = Arc::new(ProjectRt {
            project,
            base,
            lock: Default::default(),
            known: Mutex::new(known),
            gh,
            token,
            pending: Pending::default(),
            wake: Notify::new(),
            last_migrated: Mutex::new(None),
            seed_pending: AtomicBool::new(false),
            migrating: Default::default(),
            migrate_failures: Default::default(),
            ws_ok: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
            up_queued: AtomicBool::new(false),
        });
        // Replaces (and so stops) an older registration of the same checkout.
        self.projects.lock().insert(root.clone(), rt.clone());
        self.save();
        info!(
            "project {} at {} (base {}, https://{})",
            rt.project.name,
            root.display(),
            rt.base,
            rt.primary().host()
        );

        let (r, wd) = (root.clone(), rt.project.worktrees_dir());
        tokio::task::spawn_blocking(move || worktree::clean_trash(&r, &wd)).await?;
        self.reconcile(&rt).await?;
        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(worker(self.clone(), rt.clone())));
        tasks.push(tokio::spawn(ticker(rt.clone())));
        if let Some(gh) = rt.gh.clone() {
            tasks.push(tokio::spawn(watch_github(rt.clone(), gh)));
        }
        if let Some(domain) = &rt.project.settings.tls_domain {
            self.trusted.register(domain);
            tasks.push(tokio::spawn(domain_certificate(
                self.trusted.clone(),
                rt.clone(),
            )));
        }
        rt.tasks.lock().extend(tasks);
        rt.trigger(&rt.pending.migrate);
        rt.trigger(&rt.pending.merged);
        Ok(())
    }
}

// ---------- entry point

/// The PostgreSQL proxy's decision for `user` opening `db`, `c` being the registered
/// checkout whose role `user` is: Err refuses, Ok(true) lets a worktree through to its
/// dev database (created first if missing), Ok(false) lets it through as is. Fails
/// closed: users that are no registered checkout's role never reach the server.
fn pg_access(user: &str, c: Option<&Checkout>, db: &str, others: &[Checkout]) -> Result<bool> {
    if user == "postgres" {
        anyhow::bail!("connect as your checkout's role (PGUSER in the devenv shell), not postgres");
    }
    let Some(c) = c else {
        anyhow::bail!(
            "{user:?} is not the role of a checkout lazy-cow-tree serves; use PGUSER / DATABASE_URL from the devenv shell in the checkout (and `lazy-cow-tree serve` in its project)"
        );
    };
    if db == "postgres" || db == "template1" {
        return Ok(false);
    }
    // Only what it owns by name with no equal or closer claim from another checkout
    // (`config::db_owner`).
    if !config::db_owner(db, others.iter().chain([c])).is_some_and(|o| o.same(c)) {
        if c.owns_db(db) {
            let rivals: Vec<String> = others
                .iter()
                .filter(|o| !o.same(c) && o.owns_db(db))
                .map(Checkout::id)
                .collect();
            anyhow::bail!(
                "{db} is also the name of {}'s database; rename the worktree to use it",
                rivals.join(", ")
            );
        }
        let own: Vec<String> = c
            .db_kinds()
            .map(|k| {
                let test = c.test_db_of(k);
                format!("{}, {test}, {test}<N>", c.dev_db_of(k))
            })
            .collect();
        anyhow::bail!(
            "{user} may only open its own databases ({}; <N>: MIX_TEST_PARTITION), not {db}",
            own.join("; ")
        );
    }
    Ok(c.worktree.is_some() && c.dev_dbs().iter().any(|d| d == db))
}

/// One PostgreSQL cluster serves every project, so a project wanting other
/// durability (`LAZY_COW_TREE_POSTGRES_DURABLE`) than the daemon's is refused, naming
/// the projects the cluster already serves.
fn durability_check<'a>(
    daemon: bool,
    project: &Project,
    others: impl Iterator<Item = &'a Project>,
) -> Result<()> {
    if project.postgres_durable() == daemon {
        return Ok(());
    }
    let kind = |d: bool| {
        if d {
            "durable"
        } else {
            "non-durable (RAM disk, fsync off)"
        }
    };
    let others: Vec<&str> = others
        .filter(|p| p.root != project.root)
        .map(|p| p.name.as_str())
        .collect();
    let started = if others.is_empty() {
        "the project that started the daemon".to_string()
    } else {
        others.join(", ")
    };
    anyhow::bail!(
        "project {} wants a {} PostgreSQL, but the running lazy-cow-tree daemon's is {} (serving {started}); one cluster serves every project, so match dangerouslyDisableDurabilityForSpeed across them or `lazy-cow-tree down` and restart from this project",
        project.name,
        kind(project.postgres_durable()),
        kind(daemon),
    )
}

/// The redis-server a checkout's REDIS_URL reaches: its own, or its project's with
/// `LAZY_COW_TREE_REDIS_INSTANCE=shared`.
fn redis_key(project: &Project, c: &Checkout) -> String {
    if project.redis_shared() {
        format!("{}+shared", project.name)
    } else {
        c.id()
    }
}

async fn daemon_alive() -> bool {
    running_version().await.is_some()
}

/// The running daemon's version ("" before 0.4.1), None when none runs.
async fn running_version() -> Option<String> {
    let status = crate::client::get::<serde_json::Value>("/status")
        .await
        .ok()?;
    Some(status["version"].as_str().unwrap_or_default().to_string())
}

/// `version` (`major.minor.patch`, "" for unknown) is older than `than`.
fn older(version: &str, than: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|n| n.parse().unwrap_or(0)).collect() };
    parse(version) < parse(than)
}

/// `lazy-cow-tree serve`: become the daemon, or register with the running one and take
/// over when it goes away.
pub async fn serve(global: Global, project: Option<Project>) -> Result<()> {
    std::fs::create_dir_all(config::home())?;
    let mut registered = false;
    loop {
        if let Some(running) = running_version().await
            && older(&running, env!("CARGO_PKG_VERSION"))
        {
            // Projects of other repositories may run an older lazy-cow-tree: the newest
            // serves all of them (they're saved, and come back with it), so features of
            // a newer one don't silently go missing.
            info!(
                "taking over from the lazy-cow-tree {} daemon",
                if running.is_empty() {
                    "older"
                } else {
                    &running
                }
            );
            let _ = crate::client::post::<serde_json::Value>("/shutdown", &()).await;
            for _ in 0..300 {
                if !daemon_alive().await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            registered = false;
        }
        if daemon_alive().await {
            if let Some(p) = &project
                && !registered
            {
                crate::client::post::<serde_json::Value>("/projects", p).await?;
                info!(
                    "registered {} with the running lazy-cow-tree daemon",
                    p.name
                );
                registered = true;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                _ = shutdown_signal() => return Ok(()),
            }
        }
        let socket = config::socket_path();
        let _ = std::fs::remove_file(&socket);
        let listener = match UnixListener::bind(&socket) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                debug!("another daemon is starting ({e}); waiting");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            // A path over the socket limit (104 bytes on macOS), a missing directory: waiting won't help.
            Err(e) => {
                return Err(e).with_context(|| format!("control socket {}", socket.display()));
            }
        };
        return lead(global, project, listener).await;
    }
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// The HTTPS proxy and, when on, devenv's control socket. Requests before the
/// daemon exists get plain 404s.
fn start_proxy(
    global: &Global,
    routes: Routes,
    ca: Arc<Ca>,
    trusted: Arc<Trusted>,
    daemon: Arc<std::sync::OnceLock<std::sync::Weak<Daemon>>>,
) {
    let get = move || daemon.get().and_then(std::sync::Weak::upgrade);
    let dash: proxy::Dashboard = {
        let get = get.clone();
        Arc::new(move || get().map(|d| dashboard(&d)).unwrap_or_default())
    };
    let ensure: proxy::Ensure = {
        let get = get.clone();
        Arc::new(move |host| {
            let d = get();
            Box::pin(async move {
                let e = d?.ensure_server(&host).await.err()?;
                warn!("{host}: {e:#}");
                Some(format!("{e:#}"))
            })
        })
    };
    let (https, http) = (global.https_port, global.http_port);
    let reserved: crate::devenv_proxy::Reserved = {
        let routes = routes.clone();
        Arc::new(move |host| routes.serves(host))
    };
    let devenv = crate::devenv_proxy::DevenvRoutes::new(
        reserved,
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], https))),
        global.devenv_proxy_ca,
    );
    // devenv checks its proxy through plain HTTP; without it `devenv up` starts its own.
    let devenv_socket = match global.devenv_proxy_socket.as_deref() {
        Some(p) if p.as_os_str() == "off" => None,
        Some(p) => Some(p.to_path_buf()),
        None if http != 0 => Some(crate::devenv_proxy::default_control_socket()),
        None => None,
    };
    if let Some(socket) = devenv_socket {
        let devenv = devenv.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::devenv_proxy::serve(&socket, devenv).await {
                warn!("devenv projects keep their own proxy: {e:#}");
            }
        });
    }
    tokio::spawn(async move {
        if let Err(e) = proxy::serve(https, http, routes, devenv, ca, trusted, dash, ensure).await {
            error!("HTTPS proxy: {e:#}");
        }
    });
}

async fn lead(global: Global, project: Option<Project>, listener: UnixListener) -> Result<()> {
    info!(
        "lazy-cow-tree daemon {} (state in {})",
        std::process::id(),
        config::home().display()
    );
    let ca = Arc::new(Ca::load_or_create(&config::home().join("ca"))?);
    // The HTTPS proxy and devenv's control socket come up before PostgreSQL (a RAM
    // disk and initdb): `devenv up` gives a proxy it starts five seconds.
    let routes = Routes::default();
    let daemon: Arc<std::sync::OnceLock<std::sync::Weak<Daemon>>> = Default::default();
    let trusted = Arc::new(Trusted::default());
    start_proxy(&global, routes.clone(), ca, trusted.clone(), daemon.clone());
    // The real server: unix socket only, one port above the proxy's.
    let pg = Postgres::start(
        if global.postgres_durable {
            config::pg_durable_dir()
        } else {
            config::pg_dir()
        },
        global.pg_port + 1,
        global.ramdisk_mb,
        global.postgres_durable,
        global.postgres_bin.clone(),
        global.postgres_settings()?,
    )
    .await?;
    // Fatal: without it roles from before could still be superusers.
    pg.prepare_templates(&global.postgres_extensions())
        .await
        .context("preparing PostgreSQL")?;
    let d = Arc::new(Daemon {
        global: global.clone(),
        pg,
        create_lock: Default::default(),
        dev_locks: Default::default(),
        redis: Arc::new(Redis::new(crate::redis::dir(), global.redis_server.clone())),
        servers: Default::default(),
        routes: routes.clone(),
        trusted,
        projects: Mutex::new(BTreeMap::new()),
        shutdown: Notify::new(),
        up_queue: Mutex::new(Vec::new()),
    });

    let weak = Arc::downgrade(&d);
    let resolve: crate::pgproxy::Resolve = Arc::new(move |user, db, client_port| {
        let weak = weak.clone();
        Box::pin(async move {
            match weak.upgrade() {
                Some(d) => d.resolve_pg(&user, &db, client_port).await,
                None => Ok(()),
            }
        })
    });
    let (pg_port, backend) = (global.pg_port, d.pg.socket());
    tokio::spawn(async move {
        if let Err(e) = crate::pgproxy::serve(pg_port, backend, resolve).await {
            error!("PostgreSQL proxy: {e:#}");
        }
    });

    let redis = d.redis.clone();
    let redis_port = global.redis_port;
    let weak = Arc::downgrade(&d);
    let _ = redis.check.set(Arc::new(move |id, client_port| {
        let weak = weak.clone();
        Box::pin(async move {
            let d = weak.upgrade()?;
            let why = d
                .primary_misused(&id, client_port, d.global.redis_port, "REDIS_URL")
                .await;
            if let Some(w) = &why {
                warn!("{w}");
            }
            why
        })
    }));
    tokio::spawn(async move {
        if let Err(e) = redis.serve(redis_port).await {
            error!("Redis: {e:#}");
        }
    });
    let _ = daemon.set(Arc::downgrade(&d));
    // Services' `restart` policies.
    let weak = Arc::downgrade(&d);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(d) = weak.upgrade() else { break };
            d.servers.supervise().await;
            d.start_up_queued();
            d.stop_idle().await;
            if let Some(t) = d.global.redis_idle_timeout {
                d.redis.stop_idle(Duration::from_secs(t)).await;
            }
        }
    });
    let app = api(d.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!("control API: {e}");
        }
    });

    let mut projects = load_saved().projects;
    if let Some(p) = project {
        projects.retain(|q| q.root != p.root);
        projects.push(p);
    }
    for p in projects {
        if !p.root.join(".git").exists() {
            warn!("{} is gone; forgetting it", p.root.display());
            continue;
        }
        let name = p.name.clone();
        if let Err(e) = d.register(p).await {
            error!("{name}: {e:#}");
        }
    }
    d.save();

    tokio::select! {
        _ = shutdown_signal() => {}
        _ = d.shutdown.notified() => {}
    }
    info!("shutting down");
    let _ = std::fs::remove_file(config::socket_path());
    let rts: Vec<_> = std::mem::take(&mut *d.projects.lock())
        .into_values()
        .collect();
    drop(rts);
    d.servers.stop_all().await;
    d.redis.stop_all().await;
    // Let hook deletions go out.
    tokio::time::sleep(Duration::from_millis(500)).await;
    d.pg.stop().await;
    Ok(())
}

#[cfg(test)]
mod tests;
