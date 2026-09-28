//! The daemon: one per user, shared by every project that registers with it.
//!
//! - PostgreSQL on an APFS RAM disk; each worktree gets `<project>_dev_<worktree>`, a
//!   copy-on-write clone of `<project>_template`
//! - services per checkout at https://<worktree>.<service>.<project>.localhost; the
//!   first request starts one (after what it depends on) if nothing listens
//! - one Redis port; the password picks the checkout's own redis-server
//! - watches `.git/worktrees`: worktrees made by anyone (git, git-cow, Claude Code)
//!   are provisioned, deleted ones cleaned up
//! - GitHub webhook websocket (polling as fallback): pushes pull branches and merge
//!   the base branch into worktrees, which are then migrated (new worktrees are
//!   migrated once too); when the base branch moves, the migrate command runs in the
//!   primary checkout and the template is refreshed from its database; worktrees whose PR merged are removed (their
//!   processes killed)
//!
//! `localforest serve` in any project either becomes the daemon or registers its project
//! with the running one, then takes over if that one goes away.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
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
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::{net::UnixListener, sync::Notify, task::JoinHandle};
use tracing::{debug, error, info, warn};

use crate::{
    config::{self, Checkout, Global, Project},
    github, history,
    postgres::Postgres,
    proxy::{self, Routes},
    redis::Redis,
    server::Servers,
    sync::Syncer,
    tls::Ca,
    worktree::{self, Safety},
};

pub struct Daemon {
    global: Global,
    pg: Postgres,
    /// Serialises on-demand CREATE DATABASE and swapping in a new template.
    create_lock: tokio::sync::Mutex<()>,
    /// Per dev database: its first connects wait until it's cloned and adopted.
    dev_locks: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    redis: Arc<Redis>,
    servers: Servers,
    routes: Routes,
    /// Last activity per checkout (previews close after `preview_ttl_hours` without).
    activity: history::Activity,
    projects: Mutex<BTreeMap<PathBuf, Arc<ProjectRt>>>,
    shutdown: Notify,
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

struct ProjectRt {
    project: Project,
    base: String,
    /// Serialises everything that changes worktrees or databases.
    lock: tokio::sync::Mutex<()>,
    /// Provisioned worktrees by name.
    known: Mutex<BTreeMap<String, Checkout>>,
    /// Removed worktrees whose history is written but databases not yet dropped.
    recorded: Mutex<HashSet<String>>,
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
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
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
        self.migrating
            .lock()
            .unwrap()
            .entry(c.id())
            .or_default()
            .clone()
    }

    fn has_migrations(&self, c: &Checkout) -> bool {
        self.project.settings.migrate.is_some()
            || c.services.0.values().any(|s| s.migrate.is_some())
    }

    fn migrate_failure(&self, c: &Checkout) -> Option<MigrateFailure> {
        self.migrate_failures.lock().unwrap().get(&c.id()).cloned()
    }

    /// Record how migrating `c` went: None when it succeeded.
    fn migrated(&self, c: &Checkout, error: Option<String>) {
        let mut failures = self.migrate_failures.lock().unwrap();
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

    fn trigger(&self, flag: &AtomicBool) {
        flag.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
}

impl Drop for ProjectRt {
    fn drop(&mut self) {
        for t in self.tasks.lock().unwrap().drain(..) {
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
    pub services: Vec<ServiceStatus>,
    /// Its redis-server is running.
    pub redis: bool,
    pub branch: Option<String>,
    /// Its last migration failed (retried with a backoff).
    #[serde(default)]
    pub migrate_error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ServiceStatus {
    pub name: String,
    pub url: Option<String>,
    pub port: u16,
    /// Started by localforest and running.
    pub running: bool,
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
    pub projects: Vec<ProjectStatus>,
    pub pg_port: u16,
    pub redis_port: u16,
    pub https_port: u16,
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

/// Why an idle preview stays open.
enum PreviewKeep {
    /// It has work that closing would lose.
    Work(String),
    /// Can't tell right now (GitHub unreachable, …).
    Unknown(String),
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

// ---------- daemon

impl Daemon {
    fn project(&self, root: &Path) -> Result<Arc<ProjectRt>> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        self.projects
            .lock()
            .unwrap()
            .get(&root)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "{} is not registered; run `localforest serve` in it",
                    root.display()
                )
            })
    }

    fn save(&self) {
        let saved = Saved {
            projects: self
                .projects
                .lock()
                .unwrap()
                .values()
                .map(|p| p.project.clone())
                .collect(),
        };
        if let Err(e) = serde_json::to_vec_pretty(&saved)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(std::fs::write(state_file(), b)?))
        {
            warn!("saving state: {e:#}");
        }
    }

    pub async fn register(self: &Arc<Self>, project: Project) -> Result<()> {
        let root = project.root.clone();
        if let Some(existing) = self.projects.lock().unwrap().get(&root)
            && existing.project == project
        {
            return Ok(());
        }
        Repository::open(&root)
            .with_context(|| format!("{} is not a git checkout", root.display()))?;
        if let Some(other) = self
            .projects
            .lock()
            .unwrap()
            .values()
            .find(|p| p.project.name == project.name && p.project.root != root)
        {
            anyhow::bail!(
                "project name {} is taken by {}; set localforest.project (LOCALFOREST_PROJECT) to another name",
                project.name,
                other.project.root.display()
            );
        }
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
            .unwrap()
            .get(&root)
            .map(|old| old.known.lock().unwrap().clone())
            .unwrap_or_default();
        let rt = Arc::new(ProjectRt {
            project,
            base,
            lock: Default::default(),
            known: Mutex::new(known),
            recorded: Default::default(),
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
            watcher: Mutex::new(None),
        });
        // Replaces (and so stops) an older registration of the same checkout.
        self.projects
            .lock()
            .unwrap()
            .insert(root.clone(), rt.clone());
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
        self.start_watcher(&rt)?;
        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(worker(self.clone(), rt.clone())));
        tasks.push(tokio::spawn(ticker(rt.clone())));
        if let Some(gh) = rt.gh.clone() {
            tasks.push(tokio::spawn(watch_github(rt.clone(), gh)));
        }
        rt.tasks.lock().unwrap().extend(tasks);
        rt.trigger(&rt.pending.migrate);
        rt.trigger(&rt.pending.merged);
        Ok(())
    }

    fn start_watcher(self: &Arc<Self>, rt: &Arc<ProjectRt>) -> Result<()> {
        let common = Repository::open(&rt.project.root)?
            .commondir()
            .to_path_buf();
        let dir = common.join("worktrees");
        std::fs::create_dir_all(&dir)?;
        let weak = Arc::downgrade(rt);
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if res.is_ok()
                    && let Some(rt) = weak.upgrade()
                {
                    rt.trigger(&rt.pending.reconcile);
                }
            })?;
        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        // The worktree dirs too: deleting one by hand leaves git's metadata behind.
        let wt_dir = rt.project.worktrees_dir();
        if wt_dir.is_dir() {
            watcher.watch(&wt_dir, RecursiveMode::NonRecursive)?;
        }
        *rt.watcher.lock().unwrap() = Some(watcher);
        Ok(())
    }

    /// Role, routes and Redis password; for the primary also its databases. A
    /// worktree's database is created when first connected to (`resolve_pg`).
    /// Worktrees also get their environment in `.env`. Ok(true) when the primary's
    /// dev database was just created (to be seeded).
    async fn provision(&self, c: &Checkout) -> Result<bool> {
        if let Err(e) = self.migrate_role(c).await {
            warn!("{}: taking over its old role: {e:#}", c.id());
        }
        self.pg.ensure_role(&c.id(), &c.pg_password()?).await?;
        let mut created = false;
        if c.worktree.is_none() {
            let extensions = self.global.postgres_extensions();
            for db in [c.dev_db(), c.test_db()] {
                if !self.pg.exists(&db).await? {
                    self.pg.create(&db, None, Some(&c.id())).await?;
                    info!("{db}: created empty");
                    created |= db == c.dev_db();
                }
                // Made before they were listed.
                if let Err(e) = self.pg.create_extensions(&db, &extensions).await {
                    warn!("{e:#}");
                }
            }
        }
        // Databases from before: made by a role of an earlier naming scheme, or cloned
        // while checkout roles were superusers (objects still the primary's). Fails
        // provisioning (retried by the next reconcile) rather than leave a checkout
        // that can't migrate.
        self.adopt_databases(c)
            .await
            .with_context(|| format!("{}: handing its databases to its role", c.id()))?;
        for (_, host, port) in c.routes() {
            self.routes.set(host, port, c.worktree.is_some(), c.id());
        }
        self.redis.allow(&c.id());
        if c.worktree.is_some() {
            let (path, env) = (c.path.clone(), c.env(&self.global));
            if let Err(e) =
                tokio::task::spawn_blocking(move || worktree::write_env(&path, &env)).await?
            {
                warn!("{}: writing .env: {e:#}", c.id());
            }
        }
        Ok(created)
    }

    /// Upgrade: a worktree whose name changed when names started coming from git
    /// admin dirs keeps its databases (renamed) and preview record (its old role:
    /// `migrate_role`, via `legacy_id`). Skipped for anything another checkout
    /// (`others`) uses under that name.
    async fn adopt_legacy(&self, rt: &ProjectRt, c: &Checkout, others: &[Checkout]) -> Result<()> {
        let Some(old) = c.legacy() else {
            return Ok(());
        };
        let taken = |db: &str| others.iter().any(|o| o.owns_db(db));
        if others
            .iter()
            .any(|o| o.project == c.project && o.worktree.as_deref() == Some(old.name.as_str()))
        {
            return Ok(());
        }
        let dbs = self.pg.databases().await?;
        for (from, to) in &old.dbs {
            if from == to || !dbs.contains(from) || taken(from) {
                continue;
            }
            if dbs.contains(to) {
                self.pg.drop(from).await?;
                info!("dropped {from}: {to} already exists");
            } else {
                self.pg.rename(from, to).await?;
                info!("renamed database {from} to {to}");
            }
        }
        for db in dbs.iter().filter(|d| old.owns_partition(d) && !taken(d)) {
            self.pg.drop(db).await?;
            info!("dropped old test database {db}");
        }
        if let Some(new) = c.worktree.clone() {
            history::History::update(&rt.project.root, |h| {
                if let Some(p) = h.previews.remove(&old.name) {
                    h.previews.entry(new).or_insert(p);
                }
            })?;
        }
        Ok(())
    }

    /// Upgrade from `<project>-<worktree>` role names: the worktree's old role becomes
    /// its new one (renamed: it keeps what it owns). When the old name is ambiguous
    /// (another checkout's id, or another's old id: the collision this naming fixes),
    /// the old role stays and the new one becomes a member of it instead, so it may
    /// still use the objects the old one owns.
    async fn migrate_role(&self, c: &Checkout) -> Result<()> {
        let Some(old) = c.legacy_id() else {
            return Ok(());
        };
        if !self.pg.role_exists(&old).await? {
            return Ok(());
        }
        let projects: Vec<Project> = self
            .projects
            .lock()
            .unwrap()
            .values()
            .map(|rt| rt.project.clone())
            .collect();
        let me = c.path.clone();
        let others = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for p in projects {
                out.push(p.checkout(None, p.root.clone()));
                for i in worktree::list(&p.root).unwrap_or_default() {
                    if i.path != me {
                        out.push(p.checkout(Some(&i.name), i.path));
                    }
                }
            }
            out
        })
        .await?;
        let shared = others
            .iter()
            .any(|o| o.id() == old || o.legacy_id().as_deref() == Some(old.as_str()));
        let new = c.id();
        match (shared, self.pg.role_exists(&new).await?) {
            (false, false) => {
                self.pg.rename_role(&old, &new).await?;
                info!("renamed role {old} to {new}");
            }
            (false, true) => {
                self.pg.retire_role(&old, &new).await?;
                info!("moved role {old}'s objects to {new}");
            }
            (true, _) => {
                self.pg.ensure_role(&new, &c.pg_password()?).await?;
                self.pg.grant_role(&old, &new).await?;
                info!("{new}: member of {old}, which another checkout also used");
            }
        }
        Ok(())
    }

    /// A worktree seen for the first time: clone build caches into it when it was made
    /// by plain `git worktree add`. Before `provision`, which writes its `.env`.
    async fn carry_caches(&self, rt: &ProjectRt, info: &worktree::Info, c: &Checkout) {
        let (root, i) = (rt.project.root.clone(), info.clone());
        match tokio::task::spawn_blocking(move || worktree::carry_caches(&root, &i)).await {
            Ok(Err(e)) => warn!("{}: cloning caches: {e:#}", c.id()),
            Err(e) => warn!("{}: {e}", c.id()),
            Ok(Ok(_)) => {}
        }
    }

    /// Run the setup command once in a new worktree, after `provision` (its role and
    /// `.env` exist, so setup may use the database).
    async fn run_setup(&self, rt: &ProjectRt, info: &worktree::Info, c: &Checkout) {
        let Some(cmd) = rt.project.settings.setup.clone() else {
            return;
        };
        // Marked in the worktree's git admin dir, which goes away with it.
        let Some(marker) = Repository::open(&info.path)
            .ok()
            .map(|r| r.path().join("localforest-setup"))
        else {
            return;
        };
        if marker.exists() {
            return;
        }
        let id = c.run_id("setup");
        match self
            .run_command(rt, &id, &cmd, &c.path, c.env(&self.global))
            .await
        {
            Ok(()) => {
                let _ = std::fs::write(&marker, history::now().to_string());
            }
            Err(e) => warn!("{e:#}"),
        }
    }

    /// Create a worktree's dev database if missing: a copy-on-write clone of the
    /// template, or of the primary's while there's no template and it's idle.
    /// Ok(true) when it was created.
    async fn ensure_dev_db(&self, rt: &ProjectRt, c: &Checkout) -> Result<bool> {
        let dev = c.dev_db();
        // This database's connects wait until it's cloned and adopted; others only
        // for the clone itself (create_lock), not for `adopt`'s object locks.
        let lock = self
            .dev_locks
            .lock()
            .unwrap()
            .entry(dev.clone())
            .or_default()
            .clone();
        let _dev = lock.lock().await;
        if self.pg.exists(&dev).await? {
            return Ok(false);
        }
        let create = self.create_lock.lock().await;
        let template = c.template_db();
        let primary = rt.primary().dev_db();
        let source = if self.pg.exists(&template).await? {
            Some(template)
        } else if self.pg.exists(&primary).await? && self.pg.connections(&primary).await? == 0 {
            Some(primary)
        } else {
            None
        };
        let t = std::time::Instant::now();
        self.pg
            .create(&dev, source.as_deref(), Some(&c.id()))
            .await?;
        drop(create);
        if let Err(e) = self.adopt_dev_db(c).await {
            // Cloned objects its role can't migrate: better none at all.
            let _ = self.pg.drop(&dev).await;
            return Err(e);
        }
        match &source {
            Some(s) => info!("{dev}: cloned from {s} in {:?}", t.elapsed()),
            None => info!("{dev}: created empty (no template yet)"),
        }
        Ok(true)
    }

    /// Migrate a worktree's dev database (creating it first if missing) unless it
    /// already was: its branch may carry migrations the template lacks. Done is a
    /// marker in the worktree's git admin dir holding the database's OID, written
    /// only on success, so a database made by an early connection, a failed run or
    /// a database recreated after a reboot all count as not done. `force` runs them
    /// anyway (the base branch was merged in) and ignores the retry backoff.
    /// Holds only the checkout's migrate lock, which its services wait on.
    async fn migrate_worktree(&self, rt: &ProjectRt, c: &Checkout, force: bool) -> Result<()> {
        if !rt.has_migrations(c) {
            return Ok(());
        }
        let lock = rt.migrate_lock(c);
        let _g = lock.lock().await;
        if !force && let Some(f) = rt.migrate_failure(c).filter(MigrateFailure::backing_off) {
            anyhow::bail!("{} (retried later)", f.error);
        }
        self.ensure_dev_db(rt, c).await?;
        let oid = self
            .pg
            .oid(&c.dev_db())
            .await?
            .ok_or_else(|| anyhow!("{} vanished", c.dev_db()))?
            .to_string();
        let marker = Repository::open(&c.path)
            .map(|r| r.path().join("localforest-migrated"))
            .ok();
        if !force
            && marker
                .as_ref()
                .and_then(|m| std::fs::read_to_string(m).ok())
                .is_some_and(|m| m.trim() == oid)
        {
            return Ok(());
        }
        let result = self.run_migrations(rt, c).await;
        if result.is_ok()
            && let Some(m) = &marker
            && let Err(e) = std::fs::write(m, &oid)
        {
            warn!("{}: {}: {e}", c.id(), m.display());
        }
        rt.migrated(c, result.as_ref().err().map(|e| format!("{e:#}")));
        result
    }

    /// Migrate every worktree whose database isn't yet (outside `rt.lock`); not
    /// while the primary's fresh database awaits `migrate`, which triggers this after.
    async fn migrate_worktrees(&self, rt: &ProjectRt) {
        if rt.seed_pending.load(Ordering::SeqCst) {
            return;
        }
        let known: Vec<Checkout> = rt.known.lock().unwrap().values().cloned().collect();
        for c in &known {
            if rt.migrate_failure(c).is_some_and(|f| f.backing_off()) {
                continue;
            }
            if let Err(e) = self.migrate_worktree(rt, c, false).await {
                warn!("{}: {e:#}", c.id());
            }
        }
    }

    /// Give a worktree's role the objects its dev database was cloned with: owned by the
    /// template's owner role, or by the primary's role (cloned from its database, or
    /// before templates had their own).
    async fn adopt_dev_db(&self, c: &Checkout) -> Result<()> {
        let primary = Checkout {
            worktree: None,
            ..c.clone()
        };
        if c.worktree.is_some() {
            for from in [c.template_db(), primary.id()] {
                self.pg.adopt(&c.dev_db(), &from, &c.id()).await?;
            }
        }
        Ok(())
    }

    /// Hand the checkout's role the databases it owns by name (`config::db_owner`) but
    /// another, unregistered role made: e.g. its role under an earlier naming scheme,
    /// with everything in them. Then `adopt_dev_db`.
    async fn adopt_databases(&self, c: &Checkout) -> Result<()> {
        let others = self.all_checkouts();
        for db in self.pg.databases().await? {
            if !c.owns_db(&db)
                || !config::db_owner(&db, others.iter().chain([c])).is_some_and(|o| o.same(c))
            {
                continue;
            }
            let Some(old) = self.pg.owner(&db).await? else {
                continue;
            };
            if old == c.id() || old == "postgres" || others.iter().any(|o| o.id() == old) {
                continue;
            }
            self.pg.set_owner(&db, &c.id()).await?;
            self.pg.adopt(&db, &old, &c.id()).await?;
            info!("{db}: handed from {old} to {}", c.id());
        }
        self.adopt_dev_db(c).await
    }

    /// PostgreSQL proxy, before the connection is handed to the server (which checks
    /// the password): a checkout's role may open its own databases (its dev database is
    /// created on the spot) and the maintenance ones, unless another checkout's role
    /// made it. Every other user is refused.
    async fn resolve_pg(&self, user: &str, db: &str) -> Result<()> {
        let projects: Vec<Arc<ProjectRt>> =
            self.projects.lock().unwrap().values().cloned().collect();
        let found = projects.into_iter().find_map(|rt| {
            let primary = rt.primary();
            let c = if primary.id() == user {
                Some(primary)
            } else {
                rt.known
                    .lock()
                    .unwrap()
                    .values()
                    .find(|c| c.id() == user)
                    .cloned()
            };
            c.map(|c| (rt, c))
        });
        let others = self.all_checkouts();
        let create_dev = pg_access(user, found.as_ref().map(|(_, c)| c), db, &others)?;
        if let Some((rt, c)) = found {
            self.activity.touch(&c.id());
            // A database squatted by another checkout's role (CREATEDB lets it make
            // any name) would hand it this checkout's data.
            if db != "postgres"
                && db != "template1"
                && let Some(owner) = self.pg.owner(db).await?
                && owner != c.id()
                && others.iter().any(|o| o.id() == owner)
            {
                anyhow::bail!("{db} belongs to {owner}, not {user}; drop it from {owner}");
            }
            if create_dev {
                self.ensure_dev_db(&rt, &c).await?;
            }
        }
        Ok(())
    }

    /// Every registered checkout, without their runtimes.
    fn all_checkouts(&self) -> Vec<Checkout> {
        self.checkouts().into_iter().map(|(_, o)| o).collect()
    }

    /// Kill the checkout's services and redis-server, drop its routes, databases and role.
    async fn deprovision(&self, c: &Checkout) -> Result<()> {
        for (_, host, _) in c.routes() {
            self.routes.remove(&host);
        }
        self.servers.stop_checkout(c).await;
        self.redis.remove(&c.id()).await;
        for rt in self.projects.lock().unwrap().values() {
            rt.migrate_failures.lock().unwrap().remove(&c.id());
            rt.migrating.lock().unwrap().remove(&c.id());
        }
        // Only what's unambiguously its own: its name (no registered checkout has an
        // equal or closer claim) and made by its role, so a database another checkout
        // (unregistered, failed to provision) could claim by name survives.
        let others = self.all_checkouts();
        for db in self.pg.databases().await? {
            if !c.owns_db(&db) {
                continue;
            }
            if !config::db_owner(&db, others.iter().chain([c])).is_some_and(|o| o.same(c)) {
                warn!("{db}: another checkout claims it too; left in place");
                continue;
            }
            if self.pg.owner(&db).await?.as_deref() != Some(&c.id()) {
                warn!("{db}: not made by {}; left in place", c.id());
                continue;
            }
            self.pg.drop(&db).await?;
            info!("dropped database {db}");
        }
        self.pg.drop_role(&c.id()).await?;
        Ok(())
    }

    /// Provision new worktrees, clean up vanished ones.
    async fn reconcile(&self, rt: &ProjectRt) -> Result<()> {
        let _g = rt.lock.lock().await;
        self.reconcile_locked(rt).await
    }

    async fn reconcile_locked(&self, rt: &ProjectRt) -> Result<()> {
        let root = rt.project.root.clone();
        let infos = tokio::task::spawn_blocking(move || worktree::list(&root)).await??;
        if self.provision(&rt.primary()).await? {
            rt.seed_pending.store(true, Ordering::SeqCst);
            rt.trigger(&rt.pending.migrate);
        }
        // `git worktree add` still checking out: look again shortly.
        let (root, list) = (rt.project.root.clone(), infos.clone());
        let initializing: Vec<String> = tokio::task::spawn_blocking(move || {
            list.iter()
                .filter(|i| worktree::initializing(&root, i))
                .map(|i| i.name.clone())
                .collect()
        })
        .await?;
        if !initializing.is_empty() {
            rt.trigger(&rt.pending.reconcile);
        }
        let ports = self.assign_ports().await?;
        let current: BTreeMap<String, Checkout> = infos
            .iter()
            .map(|i| (i.name.clone(), checkout_with(&rt.project, i, &ports)))
            .collect();
        let known = rt.known.lock().unwrap().clone();
        for (name, c) in &current {
            // Known with the same settings: nothing to do. Carried over from an earlier
            // registration of the project (so still connectable meanwhile) but
            // changed: provisioned again.
            if known.get(name) == Some(c) || initializing.contains(name) {
                continue;
            }
            let fresh = !known.contains_key(name);
            let info = infos.iter().find(|i| &i.name == name);
            if fresh && let Some(info) = info {
                self.carry_caches(rt, info, c).await;
            }
            match self.provision(c).await {
                Ok(_) => {
                    if fresh {
                        let others: Vec<Checkout> = self
                            .checkouts()
                            .into_iter()
                            .map(|(_, c)| c)
                            .chain(current.values().cloned())
                            .filter(|o| o.path != c.path)
                            .collect();
                        if let Err(e) = self.adopt_legacy(rt, c, &others).await {
                            warn!("worktree {name}: adopting its old databases: {e:#}");
                        }
                    }
                    info!(
                        "worktree {name}: https://{} -> 127.0.0.1:{}",
                        c.host(),
                        c.port
                    );
                    rt.known.lock().unwrap().insert(name.clone(), c.clone());
                    rt.trigger(&rt.pending.migrate_worktrees);
                    if fresh && let Some(info) = info {
                        self.run_setup(rt, info, c).await;
                    }
                }
                Err(e) => warn!("provisioning {name}: {e:#}"),
            }
        }
        for (name, c) in &known {
            if !current.contains_key(name) {
                info!("worktree {name} is gone; cleaning up");
                if let Err(e) = self.deprovision(c).await {
                    // Kept: tried again on the next reconcile.
                    warn!("cleaning up {name}: {e:#}");
                    continue;
                }
                rt.known.lock().unwrap().remove(name);
                if rt.recorded.lock().unwrap().remove(name) {
                    // Removed by us; history already says why.
                    continue;
                }
                // Its own branch (named after it) usually survives a deleted directory.
                let head = Repository::open(&rt.project.root).ok().and_then(|r| {
                    r.find_reference(&format!("refs/heads/{name}"))
                        .ok()?
                        .target()
                        .map(|o| o.to_string())
                });
                self.remember(
                    rt,
                    name,
                    head.is_some().then(|| name.clone()),
                    head,
                    history::Reason::Deleted,
                    None,
                    Vec::new(),
                );
            }
        }
        if let Some(w) = rt.watcher.lock().unwrap().as_mut() {
            let wt_dir = rt.project.worktrees_dir();
            if wt_dir.is_dir() {
                let _ = w.watch(&wt_dir, RecursiveMode::NonRecursive);
            }
        }
        Ok(())
    }
    /// Record a removed worktree in the main checkout's history; `lost`: gitignored
    /// files deleted with it.
    #[allow(clippy::too_many_arguments)]
    fn remember(
        &self,
        rt: &ProjectRt,
        name: &str,
        branch: Option<String>,
        head: Option<String>,
        reason: history::Reason,
        pr: Option<u64>,
        lost: Vec<String>,
    ) {
        let rec = history::Removed {
            branch,
            head,
            at: history::now(),
            reason,
            pr,
            preview_closed: None,
            lost: lost.clone(),
        };
        if let Err(e) = history::History::update(&rt.project.root, |h| {
            // A closed preview keeps why it was gone in the first place.
            let rec = match h.previews.remove(name) {
                Some(p) => history::Removed {
                    preview_closed: Some(history::now()),
                    lost,
                    ..p.original
                },
                None => rec,
            };
            h.removed.insert(name.to_string(), rec);
        }) {
            warn!("recording removed worktree {name}: {e:#}");
        }
    }

    async fn create_worktree(&self, req: CreateReq) -> Result<CreateResp> {
        let rt = self.project(&req.root)?;
        let resp = {
            let _g = rt.lock.lock().await;
            self.create_locked(&rt, &req.name, req.base.clone()).await?
        };
        // Before its URL is handed out; a failure shows in status and on its pages.
        let c = rt.known.lock().unwrap().get(&req.name).cloned();
        if let Some(c) = c
            && let Err(e) = self.migrate_worktree(&rt, &c, false).await
        {
            warn!("{}: {e:#}", c.id());
        }
        Ok(resp)
    }

    async fn create_locked(
        &self,
        rt: &ProjectRt,
        name: &str,
        base: Option<String>,
    ) -> Result<CreateResp> {
        let project = rt.project.clone();
        let syncer = rt.syncer();
        let n = name.to_string();
        let path = tokio::task::spawn_blocking(move || {
            worktree::create(&project, &syncer, &n, base.as_deref())
        })
        .await??;
        let info = worktree::Info {
            name: name.to_string(),
            path: path.clone(),
            branch: Some(name.to_string()),
        };
        let ports = self.assign_ports().await?;
        let c = checkout_with(&rt.project, &info, &ports);
        self.carry_caches(rt, &info, &c).await;
        self.provision(&c).await?;
        rt.known.lock().unwrap().insert(name.to_string(), c.clone());
        self.run_setup(rt, &info, &c).await;
        if let Err(e) = history::History::update(&rt.project.root, |h| {
            h.removed.remove(name);
        }) {
            warn!("{e:#}");
        }
        Ok(CreateResp {
            path,
            url: format!("https://{}", c.main_host()),
            env: c.env(&self.global),
        })
    }

    /// Bring back a removed worktree at the commit it had, to preview its branch.
    /// Previews are never auto-removed for their merged PR.
    async fn recreate_preview(&self, rt: &ProjectRt, name: &str) -> Result<()> {
        let _g = rt.lock.lock().await;
        if rt.known.lock().unwrap().contains_key(name) {
            return Ok(());
        }
        let rec = history::History::load(&rt.project.root)
            .removed
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("no removed worktree {name}"))?;
        let rec_for_preview = history::Removed {
            preview_closed: None,
            ..rec.clone()
        };
        let (root, remote, gh) = (
            rt.project.root.clone(),
            rt.project.settings.remote.clone(),
            rt.gh.is_some(),
        );
        let n = name.to_string();
        let syncer = rt.syncer();
        // The branch if it's still here, else its last commit, fetched if gc'd:
        // the branch from the remote or (GitHub) the pull request's head.
        let base = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
            let repo = Repository::open(&root)?;
            if repo.find_branch(&n, git2::BranchType::Local).is_ok() {
                return Ok(None);
            }
            let head = rec
                .head
                .clone()
                .ok_or_else(|| anyhow!("{n}'s last commit is unknown"))?;
            let oid = Oid::from_str(&head)?;
            if repo.find_commit(oid).is_err() {
                let mut refspecs = Vec::new();
                if let Some(b) = &rec.branch {
                    refspecs.push(format!("+refs/heads/{b}:refs/remotes/{remote}/{b}"));
                }
                if let (Some(pr), true) = (rec.pr, gh) {
                    refspecs.push(format!("+refs/pull/{pr}/head:refs/localforest/pull/{pr}"));
                }
                if let Err(e) = syncer.fetch_refspecs(&repo, &refspecs) {
                    warn!("fetching {n}'s commit: {e:#}");
                }
                repo.find_commit(oid)
                    .map_err(|_| anyhow!("{head:.7} is gone locally and on the remote"))?;
            }
            Ok(Some(head))
        })
        .await??;
        self.create_locked(rt, name, base).await?;
        let now = history::now();
        history::History::update(&rt.project.root, |h| {
            h.previews.insert(
                name.to_string(),
                history::Preview {
                    since: now,
                    last_active: now,
                    original: rec_for_preview,
                },
            );
        })?;
        info!("recreated worktree {name} as a preview");
        Ok(())
    }

    /// Close previews without activity for `preview_ttl_hours` (every minute; saves
    /// their last activity so it survives restarts).
    async fn close_idle_previews(&self, rt: &ProjectRt) -> Result<()> {
        let ttl = rt.project.settings.preview_ttl_hours * 3600;
        let now = history::now();
        let mut idle = Vec::new();
        history::History::update(&rt.project.root, |h| {
            for (name, p) in h.previews.iter_mut() {
                let id = rt.project.checkout(Some(name), PathBuf::new()).id();
                if let Some(t) = self.activity.get(&id) {
                    p.last_active = p.last_active.max(t);
                }
                if ttl > 0 && now.saturating_sub(p.last_active) > ttl {
                    idle.push(name.clone());
                }
            }
        })?;
        if idle.is_empty() {
            return Ok(());
        }
        let _g = rt.lock.lock().await;
        let root = rt.project.root.clone();
        let infos = tokio::task::spawn_blocking(move || worktree::list(&root)).await??;
        for name in idle {
            let Some(info) = infos.iter().find(|i| i.name == name) else {
                continue;
            };
            if worktree::is_locked(&rt.project.root, info) {
                debug!("keeping idle preview {name}: its worktree is locked");
                continue;
            }
            match self.preview_closable(rt, info).await {
                Ok(()) => {}
                Err(PreviewKeep::Work(why)) => {
                    info!("keeping idle preview {name}: {why}");
                    // Ask again after another TTL, not every minute.
                    history::History::update(&rt.project.root, |h| {
                        if let Some(p) = h.previews.get_mut(&name) {
                            p.last_active = now;
                        }
                    })?;
                    continue;
                }
                Err(PreviewKeep::Unknown(why)) => {
                    // Asked again next minute: closes once it can tell.
                    debug!("idle preview {name}: can't tell yet if it's safe to close: {why}");
                    continue;
                }
            }
            info!(
                "closing preview {name}: no activity for {} h",
                rt.project.settings.preview_ttl_hours
            );
            if let Err(e) = self
                .remove_locked(rt, info, &[], history::Reason::Removed, None)
                .await
            {
                warn!("closing preview {name}: {e:#}");
            }
        }
        Ok(())
    }

    /// Can an idle preview close without losing work? Clean, and every commit on
    /// the base branch, pushed to its branch on the remote (merged or not), or in
    /// its merged PR.
    async fn preview_closable(
        &self,
        rt: &ProjectRt,
        info: &worktree::Info,
    ) -> std::result::Result<(), PreviewKeep> {
        let unknown = |e: anyhow::Error| PreviewKeep::Unknown(format!("{e:#}"));
        let (remote, base, i) = (
            rt.project.settings.remote.clone(),
            rt.base.clone(),
            info.clone(),
        );
        let unpushed = tokio::task::spawn_blocking(move || -> Result<Option<usize>> {
            if worktree::is_dirty(&i.path)? {
                return Ok(None);
            }
            worktree::unpushed(&i, &remote, &base).map(Some)
        })
        .await
        .map_err(|e| unknown(e.into()))?
        .map_err(unknown)?;
        let n = match unpushed {
            None => return Err(PreviewKeep::Work("uncommitted changes".into())),
            Some(0) => return Ok(()),
            Some(n) => n,
        };
        let (Some(gh), Some(branch)) = (&rt.gh, &info.branch) else {
            return Err(PreviewKeep::Work(format!(
                "{n} unpushed commit(s) and no GitHub to look up a merged PR"
            )));
        };
        let Some(github::MergedPr {
            number,
            head: pr_head,
            ..
        }) = gh.merged_pr(branch).await.map_err(unknown)?
        else {
            return Err(PreviewKeep::Work(format!(
                "{n} commit(s) neither pushed nor merged"
            )));
        };
        let pr_head = Oid::from_str(&pr_head).map_err(|e| unknown(e.into()))?;
        let (remote, base, i) = (
            rt.project.settings.remote.clone(),
            rt.base.clone(),
            info.clone(),
        );
        let covered = tokio::task::spawn_blocking(move || {
            worktree::covered_by_pr(&i, pr_head, &remote, &base)
        })
        .await
        .map_err(|e| unknown(e.into()))?
        .map_err(unknown)?;
        if covered {
            Ok(())
        } else {
            Err(PreviewKeep::Work(format!(
                "commits after #{number} merged, not pushed"
            )))
        }
    }

    async fn remove_worktree(&self, req: RemoveReq) -> Result<Vec<String>> {
        let rt = self.project(&req.root)?;
        let _g = rt.lock.lock().await;
        let root = rt.project.root.clone();
        let infos = tokio::task::spawn_blocking(move || worktree::list(&root)).await??;
        let target = Path::new(&req.name);
        let info = infos
            .iter()
            .find(|i| {
                i.name == req.name
                    || (target.is_absolute() && target.canonicalize().is_ok_and(|p| p == i.path))
            })
            .cloned()
            .ok_or_else(|| anyhow!("no worktree {}", req.name))?;
        if info.branch.as_deref() == Some(rt.base.as_str()) {
            anyhow::bail!(
                "{} has {} checked out; switch it to a task branch first",
                info.name,
                rt.base
            );
        }
        let pr = if req.force {
            None
        } else {
            self.check_removable(&rt, &info).await?
        };
        let reason = if pr.is_some() {
            history::Reason::Merged
        } else {
            history::Reason::Removed
        };
        self.remove_locked(&rt, &info, &req.keep_pids, reason, pr)
            .await
    }

    /// Ok(Some(pr)) when a merged PR is what makes it safe.
    async fn check_removable(&self, rt: &ProjectRt, info: &worktree::Info) -> Result<Option<u64>> {
        let (remote, base) = (rt.project.settings.remote.clone(), rt.base.clone());
        let i = info.clone();
        let safety = tokio::task::spawn_blocking(move || worktree::safety(&i, &remote, &base))
            .await?
            .map_err(|e| anyhow!("{e:#}; pass --force to remove anyway"))?;
        let Safety::NeedsMergedPr { branch, head } = safety else {
            return Ok(None);
        };
        let gh = rt.gh.as_ref().ok_or_else(|| {
            anyhow!(
                "{branch} has commits beyond {} and GitHub is unavailable; pass --force",
                rt.base
            )
        })?;
        let github::MergedPr {
            number,
            head: pr_head,
            merged_at,
        } = gh.merged_pr(&branch).await?.ok_or_else(|| {
            anyhow!(
                "{branch} has commits beyond {} and no merged PR; merge it or pass --force",
                rt.base
            )
        })?;
        let pr_head = Oid::from_str(&pr_head)?;
        let i = info.clone();
        tokio::task::spawn_blocking(move || worktree::made_before(&i, merged_at))
            .await?
            .map_err(|e| anyhow!("{branch}: #{number} merged, but {e:#}; pass --force"))?;
        let (remote, base, i) = (
            rt.project.settings.remote.clone(),
            rt.base.clone(),
            info.clone(),
        );
        if !tokio::task::spawn_blocking(move || {
            worktree::covered_by_pr(&i, pr_head, &remote, &base)
        })
        .await??
        {
            anyhow::bail!("{branch} has commits after #{number} merged ({head:.7}); pass --force");
        }
        info!("{branch} merged as #{number}");
        Ok(Some(number))
    }

    /// Can the worktree's branch go (also after --force)? Only when every commit of
    /// it is on the base branch, pushed, or in its merged PR.
    async fn branch_disposable(&self, rt: &ProjectRt, info: &worktree::Info) -> bool {
        let (remote, base, i) = (
            rt.project.settings.remote.clone(),
            rt.base.clone(),
            info.clone(),
        );
        match tokio::task::spawn_blocking(move || worktree::unpushed(&i, &remote, &base)).await {
            Ok(Ok(0)) => return true,
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!("{}: counting unpushed commits: {e:#}", info.name),
            Err(e) => warn!("{}: counting unpushed commits: {e}", info.name),
        }
        let (Some(gh), Some(branch)) = (&rt.gh, &info.branch) else {
            return false;
        };
        let pr_head = match gh.merged_pr(branch).await {
            Ok(Some(m)) => m.head,
            Ok(None) => return false,
            Err(e) => {
                warn!("{branch}: looking up its merged PR: {e:#}");
                return false;
            }
        };
        let Ok(pr_head) = Oid::from_str(&pr_head) else {
            return false;
        };
        // As remove_merged: the PR has it all, the base branch merged in aside.
        let (remote, base, i) = (
            rt.project.settings.remote.clone(),
            rt.base.clone(),
            info.clone(),
        );
        tokio::task::spawn_blocking(move || worktree::covered_by_pr(&i, pr_head, &remote, &base))
            .await
            .is_ok_and(|r| r.is_ok_and(|covered| covered))
    }

    /// Ok(warnings about what was lost or kept, for the caller).
    async fn remove_locked(
        &self,
        rt: &ProjectRt,
        info: &worktree::Info,
        keep: &[i32],
        reason: history::Reason,
        pr: Option<u64>,
    ) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        let head = Repository::open(&info.path)
            .ok()
            .and_then(|r| r.head().ok()?.target())
            .map(|o| o.to_string());
        let p = info.path.clone();
        if let Ok(Ok(files)) = tokio::task::spawn_blocking(move || worktree::uncommitted(&p)).await
            && !files.is_empty()
        {
            let w = format!(
                "{}: deleting uncommitted {}",
                info.name,
                worktree::short_list(&files)
            );
            warn!("{w}");
            warnings.push(w);
        }
        let (root, path) = (rt.project.root.clone(), info.path.clone());
        let lost = tokio::task::spawn_blocking(move || worktree::ignored_files(&root, &path))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        if !lost.is_empty() {
            let w = format!(
                "{}: deleting gitignored {}",
                info.name,
                worktree::short_list(&lost)
            );
            warn!("{w}");
            warnings.push(w);
        }
        let c = rt.project.checkout(Some(&info.name), info.path.clone());
        self.servers.stop_checkout(&c).await;
        // Whatever else runs there: a server started by hand, iex, watchers.
        worktree::kill_processes_in(&info.path, keep).await;
        // Decided while the worktree (its HEAD) is still there.
        let delete_branch = pr.is_some() || self.branch_disposable(rt, info).await;
        // Files first: if they can't move, the databases stay with them.
        let (root, i) = (rt.project.root.clone(), info.clone());
        let kept =
            tokio::task::spawn_blocking(move || worktree::remove_files(&root, &i, delete_branch))
                .await??;
        if let Some(k) = &kept {
            warnings.push(format!(
                "{}: kept its unmerged, unpushed commits as branch {k}",
                info.name
            ));
        }
        let branch = kept.or_else(|| info.branch.clone());
        self.remember(rt, &info.name, branch, head, reason, pr, lost);
        if let Err(e) = self.deprovision(&c).await {
            // Still known: the next reconcile finds it gone and tries again.
            rt.recorded.lock().unwrap().insert(info.name.clone());
            anyhow::bail!(
                "removed {}, but not its databases yet (retrying later): {e:#}",
                info.name
            );
        }
        rt.known.lock().unwrap().remove(&info.name);
        info!("removed worktree {}", info.name);
        Ok(warnings)
    }

    /// Remove every worktree whose branch's PR merged with nothing left unmerged
    /// (except previews of merged branches).
    async fn remove_merged(&self, rt: &ProjectRt) -> Result<()> {
        let Some(gh) = rt.gh.clone() else {
            return Ok(());
        };
        if rt.project.settings.no_auto_remove {
            return Ok(());
        }
        let _g = rt.lock.lock().await;
        let root = rt.project.root.clone();
        let infos = tokio::task::spawn_blocking(move || worktree::list(&root)).await??;
        let previews = history::History::load(&rt.project.root).previews;
        let managed = rt
            .project
            .worktrees_dir()
            .canonicalize()
            .unwrap_or_default();
        for info in infos {
            let Some(branch) = info.branch.clone() else {
                continue;
            };
            if branch == rt.base
                || !info.path.starts_with(&managed)
                || previews.contains_key(&info.name)
            {
                continue;
            }
            if worktree::is_locked(&rt.project.root, &info) {
                info!(
                    "{branch}: worktree {} is locked; not auto-removing it",
                    info.name
                );
                continue;
            }
            let Some(merged) = gh.merged_pr(&branch).await? else {
                continue;
            };
            let number = merged.number;
            let Ok(pr_head) = Oid::from_str(&merged.head) else {
                continue;
            };
            // Same branch name, but made after that PR merged (a new task reusing the
            // name): not the PR's worktree, whatever its commits.
            let (i, at) = (info.clone(), merged.merged_at);
            if let Err(e) =
                tokio::task::spawn_blocking(move || worktree::made_before(&i, at)).await?
            {
                info!("{branch}: #{number} merged, keeping it: {e:#}");
                continue;
            }
            let (remote, base, i) = (
                rt.project.settings.remote.clone(),
                rt.base.clone(),
                info.clone(),
            );
            let ok = tokio::task::spawn_blocking(move || -> Result<bool> {
                Ok(!worktree::is_dirty(&i.path)?
                    && worktree::covered_by_pr(&i, pr_head, &remote, &base)?)
            })
            .await?;
            // One unreadable worktree (a shallow clone's missing parent) keeps only itself.
            let ok = ok.unwrap_or_else(|e| {
                warn!("{branch}: checking {} against #{number}: {e:#}", info.name);
                false
            });
            if !ok {
                info!("{branch}: #{number} merged, but the worktree has newer work; keeping it");
                continue;
            }
            info!(
                "{branch}: #{number} merged; removing worktree {}",
                info.name
            );
            if let Err(e) = self
                .remove_locked(rt, &info, &[], history::Reason::Merged, Some(number))
                .await
            {
                warn!("removing {}: {e:#}", info.name);
            }
        }
        Ok(())
    }

    /// Base branch moved in the primary checkout, or its dev database was just
    /// created: migrate (and seed, when fresh) its database, then make it the
    /// template new worktrees clone, and migrate worktrees without a database.
    ///
    /// A fresh database is migrated and seeded whatever branch the primary is on, or
    /// the app would face an empty one. The template, though, is only refreshed from
    /// an up-to-date base branch, even when there is none yet: a feature branch's
    /// migrations would otherwise reach every new worktree, whose own branch may not
    /// have them. Without a template (first start or reboot while on a feature
    /// branch), worktrees clone the idle primary's database instead (`ensure_dev_db`),
    /// feature-branch migrations included, until the primary is back on an
    /// up-to-date base branch and the template is made.
    ///
    /// Holds the primary's migrate lock (its services wait on it), not `rt.lock`.
    async fn migrate(&self, rt: &ProjectRt) -> Result<()> {
        let primary = rt.primary();
        let lock = rt.migrate_lock(&primary);
        let result = {
            let _g = lock.lock().await;
            self.migrate_primary(rt, &primary).await
        };
        match &result {
            Ok(false) => {}
            Ok(true) => {
                rt.migrated(&primary, None);
                rt.trigger(&rt.pending.migrate_worktrees);
            }
            Err(e) => rt.migrated(&primary, Some(format!("{e:#}"))),
        }
        result.map(|_| ())
    }

    /// `migrate`'s work; Ok(true) when it ran.
    async fn migrate_primary(&self, rt: &ProjectRt, primary: &Checkout) -> Result<bool> {
        let root = rt.project.root.clone();
        let (remote, base) = (rt.project.settings.remote.clone(), rt.base.clone());
        let head = tokio::task::spawn_blocking(move || -> Result<Option<Oid>> {
            let repo = Repository::open(&root)?;
            let head = repo.head()?;
            if head.shorthand().ok() != Some(base.as_str()) || !head.is_branch() {
                return Ok(None);
            }
            let local = head.peel_to_commit()?.id();
            if let Ok(r) = repo.find_reference(&format!("refs/remotes/{remote}/{base}"))
                && r.peel_to_commit()?.id() != local
                && !repo.graph_descendant_of(local, r.peel_to_commit()?.id())?
            {
                return Ok(None);
            }
            Ok(Some(local))
        })
        .await??;
        let fresh = rt.seed_pending.load(Ordering::SeqCst);
        match head {
            None if !fresh => {
                debug!(
                    "{}: primary checkout is not on an up-to-date {}; not migrating",
                    rt.project.name, rt.base
                );
                return Ok(false);
            }
            Some(h) if !fresh && *rt.last_migrated.lock().unwrap() == Some(h) => {
                return Ok(false);
            }
            _ => {}
        }
        match head {
            Some(h) => info!("{}: {} is at {h:.7}", rt.project.name, rt.base),
            None => info!(
                "{}: new database, primary checkout is not on an up-to-date {}; \
                 migrating it without refreshing the template",
                rt.project.name, rt.base
            ),
        }
        self.run_migrations(rt, primary).await?;
        if rt.seed_pending.swap(false, Ordering::SeqCst)
            && let Some(cmd) = &rt.project.settings.seed
        {
            // Into the template with the rest, so worktrees get seeded data.
            self.run_command(
                rt,
                &primary.run_id("seed"),
                cmd,
                &primary.path,
                primary.env(&self.global),
            )
            .await?;
        }
        if let Some(head) = head {
            let dev = primary.dev_db();
            if self.pg.exists(&dev).await? {
                let t = std::time::Instant::now();
                self.pg
                    .snapshot(&dev, &primary.template_db(), &self.create_lock)
                    .await?;
                info!(
                    "{} refreshed from {dev} in {:?}",
                    primary.template_db(),
                    t.elapsed()
                );
            }
            // Not at startup: only when the base branch moved while we watched.
            let pulled = rt.last_migrated.lock().unwrap().replace(head).is_some();
            if pulled {
                self.restart_on_pull(primary).await;
            }
        }
        Ok(true)
    }

    /// The project's migrate command (with the default service's env) and each
    /// service's own (with its env, in its cwd), in a checkout.
    async fn run_migrations(&self, rt: &ProjectRt, c: &Checkout) -> Result<()> {
        let mut runs = Vec::new();
        if let Some(cmd) = &rt.project.settings.migrate {
            runs.push((c.id(), cmd.clone(), c.path.clone(), c.env(&self.global)));
        }
        for (name, s) in &c.services.0 {
            if let Some(cmd) = &s.migrate {
                let cwd = s
                    .cwd
                    .as_ref()
                    .map_or_else(|| c.path.clone(), |d| c.path.join(d));
                runs.push((
                    c.service_id(name),
                    cmd.clone(),
                    cwd,
                    c.service_env(&self.global, Some(name)),
                ));
            }
        }
        let mut failed = Vec::new();
        for (id, cmd, cwd, env) in runs {
            if let Err(e) = self
                .run_command(rt, &format!("{id}-migrate"), &cmd, &cwd, env)
                .await
            {
                warn!("{e:#}");
                failed.push(id);
            }
        }
        if !failed.is_empty() {
            anyhow::bail!("migrations failed: {}", failed.join(", "));
        }
        Ok(())
    }

    /// Run a project command (migrate, seed, setup) with an env, logged to
    /// `logs/<id>.log`; 15 minutes at most.
    async fn run_command(
        &self,
        rt: &ProjectRt,
        id: &str,
        cmd: &str,
        cwd: &Path,
        env: Vec<(String, String)>,
    ) -> Result<()> {
        let log_path = crate::server::log_path(id);
        std::fs::create_dir_all(log_path.parent().unwrap())?;
        let log = std::fs::File::create(&log_path)?;
        info!("{id}: running `{cmd}`");
        let t = std::time::Instant::now();
        let mut command = crate::server::command(&rt.project, cmd, cwd, env)?;
        command
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true);
        let status = tokio::time::timeout(Duration::from_secs(900), command.status())
            .await
            .map_err(|_| anyhow!("`{cmd}` timed out"))??;
        if !status.success() {
            anyhow::bail!(
                "`{cmd}` failed in {id} ({status}); see {}",
                log_path.display()
            );
        }
        info!("{id}: done in {:?}", t.elapsed());
        Ok(())
    }

    /// Pull branches and merge the base branch into worktrees, then migrate the
    /// worktrees that moved (the primary goes through `migrate`).
    async fn sync(&self, rt: &ProjectRt) -> Result<()> {
        let has_remote = Repository::open(&rt.project.root)?
            .find_remote(&rt.project.settings.remote)
            .is_ok();
        if rt.project.settings.no_sync || !has_remote {
            return Ok(());
        }
        let moved = {
            let _g = rt.lock.lock().await;
            let syncer = rt.syncer();
            tokio::task::spawn_blocking(move || syncer.sync()).await??
        };
        // Outside `rt.lock`: migrations may take minutes.
        let known: Vec<Checkout> = rt.known.lock().unwrap().values().cloned().collect();
        for path in moved {
            let path = path.canonicalize().unwrap_or(path);
            if let Some(c) = known.iter().find(|c| c.path == path) {
                if let Err(e) = self.migrate_worktree(rt, c, true).await {
                    warn!("{}: {e:#}", c.id());
                }
                self.restart_on_pull(c).await;
            }
        }
        Ok(())
    }

    /// Restart the checkout's running services that ask for it after a pull.
    async fn restart_on_pull(&self, c: &Checkout) {
        for (name, s) in &c.services.0 {
            let id = c.service_id(name);
            if s.restart_on_pull && self.servers.running(&id).await {
                self.servers.restart(&id).await;
            }
        }
    }

    /// Record the ports of new worktrees of every registered project
    /// (`worktree::assign_ports`); every worktree's port by path.
    async fn assign_ports(&self) -> Result<HashMap<PathBuf, u16>> {
        let projects: Vec<Project> = self
            .projects
            .lock()
            .unwrap()
            .values()
            .map(|rt| rt.project.clone())
            .collect();
        let ports =
            tokio::task::spawn_blocking(move || worktree::assign_ports(&projects)).await??;
        Ok(ports.into_iter().collect())
    }

    /// Every checkout (primary and worktrees) of every project.
    fn checkouts(&self) -> Vec<(Arc<ProjectRt>, Checkout)> {
        let projects: Vec<Arc<ProjectRt>> =
            self.projects.lock().unwrap().values().cloned().collect();
        let mut out = Vec::new();
        for rt in projects {
            out.push((rt.clone(), rt.primary()));
            let known: Vec<Checkout> = rt.known.lock().unwrap().values().cloned().collect();
            out.extend(known.into_iter().map(|c| (rt.clone(), c)));
        }
        out
    }

    /// Start the service serving `host` (exactly, else the closest parent host; a
    /// secondary port's host starts its owner) and its dependencies if nothing listens
    /// there.
    async fn ensure_server(&self, host: &str) -> Result<()> {
        let mut best: Option<(usize, Arc<ProjectRt>, Checkout, String, u16)> = None;
        for (rt, c) in self.checkouts() {
            for (svc, h, port) in c.routes() {
                let Some(svc) = svc else { continue };
                let score = if h == host {
                    usize::MAX
                } else if host.ends_with(&format!(".{h}")) {
                    h.len()
                } else {
                    continue;
                };
                if best.as_ref().is_none_or(|b| score > b.0) {
                    best = Some((score, rt.clone(), c.clone(), svc, port));
                }
            }
        }
        match best {
            Some((_, rt, c, svc, port)) => self.ensure_service(&rt, &c, &svc, Some(port)).await,
            None => Ok(()),
        }
    }

    /// Start a service (and what it depends on) once its checkout's database is
    /// migrated: a worktree's is migrated first if it isn't yet (an error if that
    /// fails); the primary's services wait for a migration in progress.
    async fn ensure_service(
        &self,
        rt: &ProjectRt,
        c: &Checkout,
        svc: &str,
        port: Option<u16>,
    ) -> Result<()> {
        if c.worktree.is_some() {
            self.migrate_worktree(rt, c, false)
                .await
                .context("migrations failed; not starting its services")?;
        } else {
            drop(rt.migrate_lock(c).lock().await);
        }
        match port {
            Some(port) => {
                self.servers
                    .ensure_port(&rt.project, c, svc, port, &self.global)
                    .await
            }
            None => self.servers.ensure(&rt.project, c, svc, &self.global).await,
        }
    }

    /// A checkout by worktree name (None: the primary) and service (None: default).
    fn service_named(
        &self,
        root: &Path,
        worktree: Option<&str>,
        service: Option<&str>,
    ) -> Result<(Arc<ProjectRt>, Checkout, String)> {
        let rt = self.project(root)?;
        let c = match worktree {
            None => rt.primary(),
            Some(w) => rt
                .known
                .lock()
                .unwrap()
                .get(w)
                .cloned()
                .ok_or_else(|| anyhow!("no worktree {w}"))?,
        };
        let svc = match service {
            Some(s) => s.to_string(),
            None => c
                .services
                .default_name()
                .ok_or_else(|| {
                    anyhow!("no services configured (localforest.services in devenv.nix)")
                })?
                .to_string(),
        };
        if c.service(&svc).is_none() {
            anyhow::bail!("no service {svc}");
        }
        Ok((rt, c, svc))
    }

    async fn status(&self) -> Result<Status> {
        let projects: Vec<Arc<ProjectRt>> =
            self.projects.lock().unwrap().values().cloned().collect();
        let dbs = self.pg.databases().await.unwrap_or_default();
        let mut out = Vec::new();
        for rt in projects {
            let root = rt.project.root.clone();
            let infos = tokio::task::spawn_blocking(move || worktree::list(&root))
                .await?
                .unwrap_or_default();
            let primary_branch = Repository::open(&rt.project.root)
                .ok()
                .and_then(|r| r.head().ok()?.shorthand().map(str::to_string).ok());
            let mut checkouts = vec![(rt.primary(), primary_branch)];
            for i in infos {
                checkouts.push((
                    rt.project.checkout(Some(&i.name), i.path.clone()),
                    i.branch.clone(),
                ));
            }
            let mut statuses = Vec::new();
            for (c, branch) in checkouts {
                let mut services = Vec::new();
                for (name, s) in &c.services.0 {
                    services.push(ServiceStatus {
                        name: name.clone(),
                        url: s.http.then(|| format!("https://{}", c.service_host(name))),
                        port: c.service_port(name),
                        running: self.servers.running(&c.service_id(name)).await,
                    });
                }
                statuses.push(CheckoutStatus {
                    url: format!("https://{}", c.main_host()),
                    databases: dbs.iter().filter(|d| c.owns_db(d)).cloned().collect(),
                    services,
                    redis: self.redis.running(&c.id()),
                    branch,
                    migrate_error: rt.migrate_failure(&c).map(|f| f.error),
                    checkout: c,
                });
            }
            let checkouts = statuses;
            out.push(ProjectStatus {
                project: rt.project.clone(),
                base: rt.base.clone(),
                github: rt.gh.as_ref().map(|g| g.repo.to_string()),
                websocket: rt.ws_ok.load(Ordering::SeqCst),
                checkouts,
            });
        }
        Ok(Status {
            pid: std::process::id(),
            projects: out,
            pg_port: self.global.pg_port,
            redis_port: self.global.redis_port,
            https_port: self.global.https_port,
        })
    }
}

fn github_client(project: &Project) -> Result<(github::Client, String)> {
    let url = Repository::open(&project.root)?
        .find_remote(&project.settings.remote)?
        .url()
        .map(str::to_string)
        .unwrap_or_default();
    let repo =
        github::parse_remote_url(&url).ok_or_else(|| anyhow!("{url:?} is not a GitHub remote"))?;
    let token = github::auth_token(&repo.host)?;
    Ok((github::Client::new(repo, token.clone())?, token))
}

/// Runs pending jobs for one project, debounced so bursts collapse.
async fn worker(d: Arc<Daemon>, rt: Arc<ProjectRt>) {
    loop {
        rt.wake.notified().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let take = |f: &AtomicBool| f.swap(false, Ordering::SeqCst);
        // Merged worktrees go before sync merges the base branch into them.
        if take(&rt.pending.merged)
            && let Err(e) = d.remove_merged(&rt).await
        {
            warn!("{}: checking merged PRs: {e:#}", rt.project.name);
        }
        if take(&rt.pending.sync) {
            if let Err(e) = d.sync(&rt).await {
                error!("{}: sync failed: {e:#}", rt.project.name);
            }
            rt.pending.migrate.store(true, Ordering::SeqCst);
        }
        if take(&rt.pending.migrate)
            && let Err(e) = d.migrate(&rt).await
        {
            error!("{}: migrate: {e:#}", rt.project.name);
        }
        if take(&rt.pending.reconcile)
            && let Err(e) = d.reconcile(&rt).await
        {
            warn!("{}: {e:#}", rt.project.name);
        }
        if take(&rt.pending.migrate_worktrees) {
            d.migrate_worktrees(&rt).await;
        }
    }
}

/// Safety net for missed events: every minute without the websocket, else every 10.
async fn ticker(rt: Arc<ProjectRt>) {
    let mut n: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        n += 1;
        // Retries failed migrations once their backoff is over.
        if !rt.migrate_failures.lock().unwrap().is_empty() {
            rt.trigger(&rt.pending.migrate_worktrees);
        }
        // Retries checkouts whose provisioning failed.
        rt.trigger(&rt.pending.reconcile);
        if !rt.ws_ok.load(Ordering::SeqCst) || n.is_multiple_of(10) {
            rt.pending.merged.store(true, Ordering::SeqCst);
            rt.trigger(&rt.pending.sync);
        }
    }
}

/// Keep a webhook websocket open, recreating the hook on disconnect.
async fn watch_github(rt: Arc<ProjectRt>, gh: Arc<github::Client>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let hook = match gh.create_hook().await {
            Ok(h) => h,
            Err(e) => {
                rt.ws_ok.store(false, Ordering::SeqCst);
                warn!(
                    "{}: {e:#}; polling every minute instead, retrying in 10 min",
                    rt.project.name
                );
                tokio::time::sleep(Duration::from_secs(600)).await;
                continue;
            }
        };
        let guard = HookGuard {
            client: gh.clone(),
            hook: Some(hook),
        };
        rt.trigger(&rt.pending.sync);
        let started = tokio::time::Instant::now();
        let base_ref = format!("refs/heads/{}", rt.base);
        rt.ws_ok.store(true, Ordering::SeqCst);
        let result = gh
            .stream_events(guard.hook(), |ev| match ev {
                github::Event::Push(r) => {
                    info!("{}: push to {r}", rt.project.name);
                    if r == base_ref {
                        rt.pending.merged.store(true, Ordering::SeqCst);
                    }
                    rt.trigger(&rt.pending.sync);
                }
                github::Event::Merged { number, branch } => {
                    info!("{}: #{number} ({branch}) merged", rt.project.name);
                    rt.trigger(&rt.pending.merged);
                }
            })
            .await;
        rt.ws_ok.store(false, Ordering::SeqCst);
        guard.cleanup().await;
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        match result {
            Ok(()) => warn!(
                "{}: websocket closed; reconnecting in {backoff:?}",
                rt.project.name
            ),
            Err(e) => warn!(
                "{}: websocket: {e:#}; reconnecting in {backoff:?}",
                rt.project.name
            ),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

/// Deletes the hook on cleanup, or in the background when dropped (task aborted).
struct HookGuard {
    client: Arc<github::Client>,
    hook: Option<github::Hook>,
}

impl HookGuard {
    fn hook(&self) -> &github::Hook {
        self.hook.as_ref().expect("hook present until cleanup")
    }

    async fn cleanup(mut self) {
        if let Some(h) = self.hook.take()
            && let Err(e) = self.client.delete_hook(&h).await
        {
            warn!("{e:#}");
        }
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        if let Some(h) = self.hook.take() {
            let client = self.client.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    if let Err(e) = client.delete_hook(&h).await {
                        warn!("{e:#}");
                    }
                });
            }
        }
    }
}

// ---------- control API

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, format!("{:#}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

fn api(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/status", get(|State(d): State<Arc<Daemon>>| async move { ApiResult::Ok(Json(d.status().await?)) }))
        .route(
            "/projects",
            post(|State(d): State<Arc<Daemon>>, Json(p): Json<Project>| async move {
                d.register(p).await?;
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/projects/remove",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<RootReq>| async move {
                let root = r.root.canonicalize().unwrap_or(r.root);
                d.projects.lock().unwrap().remove(&root);
                d.save();
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/worktrees",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<CreateReq>| async move {
                ApiResult::Ok(Json(d.create_worktree(r).await?))
            }),
        )
        .route(
            "/worktrees/remove",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<RemoveReq>| async move {
                let warnings = d.remove_worktree(r).await?;
                ApiResult::Ok(Json(serde_json::json!({ "warnings": warnings })))
            }),
        )
        .route(
            "/sync",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<RootReq>| async move {
                let rt = d.project(&r.root)?;
                d.remove_merged(&rt).await?;
                d.sync(&rt).await?;
                d.migrate(&rt).await?;
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/db/snapshot",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<RootReq>| async move {
                let rt = d.project(&r.root)?;
                let _g = rt.lock.lock().await;
                let primary = rt.primary();
                d.pg
                    .snapshot(&primary.dev_db(), &primary.template_db(), &d.create_lock)
                    .await?;
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/service/start",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<ServiceReq>| async move {
                let (rt, c, svc) =
                    d.service_named(&r.root, r.worktree.as_deref(), r.service.as_deref())?;
                d.ensure_service(&rt, &c, &svc, None).await?;
                ApiResult::Ok(Json(serde_json::json!({
                    "log": crate::server::log_path(&c.service_id(&svc)),
                    "url": c.service(&svc).filter(|s| s.http).map(|_| format!("https://{}", c.service_host(&svc))),
                })))
            }),
        )
        .route(
            "/service/stop",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<ServiceReq>| async move {
                let (_, c, svc) =
                    d.service_named(&r.root, r.worktree.as_deref(), r.service.as_deref())?;
                d.servers.stop(&c.service_id(&svc)).await;
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .route(
            "/service/log",
            post(|State(d): State<Arc<Daemon>>, Json(r): Json<ServiceReq>| async move {
                let (_, c, svc) =
                    d.service_named(&r.root, r.worktree.as_deref(), r.service.as_deref())?;
                ApiResult::Ok(Json(serde_json::json!({
                    "log": crate::server::log_path(&c.service_id(&svc)),
                })))
            }),
        )
        .route(
            "/shutdown",
            post(|State(d): State<Arc<Daemon>>| async move {
                d.shutdown.notify_one();
                ApiResult::Ok(Json(serde_json::json!({})))
            }),
        )
        .with_state(d)
}

/// POSTed by the gone page's button.
const RECREATE_PATH: &str = "/.localforest/recreate";

impl Daemon {
    /// The project and removed worktree a hostname belonged to:
    /// `<wt>.<service>.<project>.localhost`, `<wt>.<project>.localhost` without
    /// services, or a subdomain of those.
    fn removed_for_host(&self, host: &str) -> Option<(Arc<ProjectRt>, String, history::Removed)> {
        let projects: Vec<Arc<ProjectRt>> =
            self.projects.lock().unwrap().values().cloned().collect();
        for rt in projects {
            let Some(prefix) = host.strip_suffix(&format!(".{}.localhost", rt.project.name)) else {
                continue;
            };
            let labels: Vec<&str> = prefix.split('.').collect();
            let back = if rt.project.settings.services.0.is_empty() {
                1
            } else {
                2
            };
            let Some(wt) = labels.len().checked_sub(back).map(|i| labels[i]) else {
                continue;
            };
            if rt.known.lock().unwrap().contains_key(wt) {
                continue;
            }
            let rec = history::History::load(&rt.project.root)
                .removed
                .get(wt)
                .cloned()?;
            return Some((rt, wt.to_string(), rec));
        }
        None
    }

    /// Requests to hosts without a route: a removed worktree's hostname gets the gone
    /// page (503), whose button recreates it as a preview.
    async fn unrouted(&self, u: proxy::Unrouted) -> Option<proxy::Page> {
        let (rt, name, rec) = self.removed_for_host(&u.host)?;
        let mut error = None;
        if u.method == hyper::Method::POST && u.path == RECREATE_PATH {
            // Only from the page itself (a browser always sends Origin on a form POST).
            let origin_host = u.origin.as_deref().map(|o| {
                o.trim_start_matches("https://")
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            });
            if origin_host.is_some_and(|h| h != u.host) {
                return Some(proxy::Page {
                    status: StatusCode::FORBIDDEN,
                    html: "cross-origin request refused".into(),
                    location: None,
                });
            }
            match self.recreate_preview(&rt, &name).await {
                Ok(()) => {
                    return Some(proxy::Page {
                        status: StatusCode::SEE_OTHER,
                        html: String::new(),
                        location: Some("/".into()),
                    });
                }
                Err(e) => {
                    warn!("recreating {name}: {e:#}");
                    error = Some(format!("{e:#}"));
                }
            }
        }
        let remote_url = Repository::open(&rt.project.root).ok().and_then(|r| {
            r.find_remote(&rt.project.settings.remote)
                .ok()?
                .url()
                .ok()
                .map(str::to_string)
        });
        let forge = remote_url.as_deref().and_then(history::Forge::from_remote);
        Some(proxy::Page {
            status: StatusCode::SERVICE_UNAVAILABLE,
            html: gone_page(&name, &rec, forge.as_ref(), error.as_deref()),
            location: None,
        })
    }
}

fn ago(at: u64) -> String {
    let s = history::now().saturating_sub(at);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

fn gone_page(
    name: &str,
    rec: &history::Removed,
    forge: Option<&history::Forge>,
    error: Option<&str>,
) -> String {
    let e = html_escape;
    let link = |href: &str, text: &str| format!("<a href=\"{}\">{}</a>", e(href), e(text));
    let headline = match (rec.reason, rec.pr) {
        (history::Reason::Merged, Some(n)) => match forge {
            Some(f) => format!("Its {} was merged", link(&f.pr(n), &f.pr_label(n))),
            None => format!("Its pull request #{n} was merged"),
        },
        (history::Reason::Merged, None) => "Its branch was merged".into(),
        (history::Reason::Removed, _) => "It was removed".into(),
        (history::Reason::Deleted, _) => "Its directory was deleted".into(),
    };
    let mut links = Vec::new();
    if let Some(f) = forge {
        if let Some(b) = &rec.branch {
            links.push(link(&f.branch(b), &format!("branch {b}")));
        }
        if let Some(h) = &rec.head {
            links.push(link(
                &f.commit(h),
                &format!("commit {}", &h[..h.len().min(7)]),
            ));
        }
        links.push(link(&f.repo, &format!("repository on {}", f.name)));
    } else {
        if let Some(b) = &rec.branch {
            links.push(format!("branch <code>{}</code>", e(b)));
        }
        if let Some(h) = &rec.head {
            links.push(format!("commit <code>{}</code>", e(&h[..h.len().min(7)])));
        }
    }
    let can_recreate = rec.head.is_some() || rec.branch.is_some();
    let button = if can_recreate {
        format!(
            r#"<form method="post" action="{RECREATE_PATH}" onsubmit="this.querySelector('button').disabled=true;this.querySelector('button').textContent='Recreating…'">
<button type="submit">Recreate worktree to preview {}</button></form>
<p class="note">Checks out the commit it was at, with a fresh copy of the database; its services start on the first request. A preview isn't removed for its merged pull request: remove it with <code>localforest worktree rm {}</code>.</p>"#,
            e(name),
            e(name)
        )
    } else {
        "<p class=\"note\">Its last commit is unknown, so it can't be recreated.</p>".into()
    };
    let mut error = error
        .map(|m| format!("<p class=\"error\">Recreating failed: {}</p>", e(m)))
        .unwrap_or_default();
    if !rec.lost.is_empty() {
        error = format!(
            "<p class=\"muted\">Deleted with it (gitignored): {}</p>{error}",
            e(&rec.lost.join(", "))
        );
    }
    if let Some(at) = rec.preview_closed {
        error = format!(
            "<p class=\"muted\">A preview of it was closed {} for inactivity.</p>{error}",
            ago(at)
        );
    }
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{name} is gone</title>
<style>
:root{{--bg:#fafaf9;--fg:#1c1917;--muted:#78716c;--card:#fff;--line:#e7e5e4;--accent:#2563eb;--accent-fg:#fff;--err:#b91c1c}}
@media (prefers-color-scheme:dark){{:root{{--bg:#0c0a09;--fg:#e7e5e4;--muted:#a8a29e;--card:#1c1917;--line:#292524;--accent:#60a5fa;--accent-fg:#0c0a09;--err:#f87171}}}}
body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,sans-serif}}
main{{max-width:34rem;margin:1rem;padding:2rem;background:var(--card);border:1px solid var(--line);border-radius:12px}}
.tag{{display:inline-block;font:600 12px/1 system-ui;letter-spacing:.05em;text-transform:uppercase;color:var(--muted)}}
h1{{margin:.4rem 0 .2rem;font-size:1.5rem}} h1 code{{font-size:1.3rem}}
p{{margin:.5rem 0}} .muted,.note{{color:var(--muted)}} .note{{font-size:.9rem}}
ul{{padding-left:1.2rem}} a{{color:var(--accent)}}
button{{margin-top:1rem;padding:.7rem 1.1rem;border:0;border-radius:8px;background:var(--accent);color:var(--accent-fg);font:600 1rem system-ui;cursor:pointer}}
button:disabled{{opacity:.6;cursor:wait}} .error{{color:var(--err)}}
</style></head><body><main>
<span class="tag">503 · worktree removed</span>
<h1>Worktree <code>{name}</code> is gone</h1>
<p>{headline} <span class="muted">({ago})</span>.</p>
<ul>{links}</ul>
{error}{button}
</main></body></html>"#,
        name = e(name),
        ago = ago(rec.at),
        links = links
            .iter()
            .map(|l| format!("<li>{l}</li>"))
            .collect::<String>(),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn dashboard(d: &Daemon) -> String {
    let mut rows = String::new();
    let projects: Vec<Arc<ProjectRt>> = d.projects.lock().unwrap().values().cloned().collect();
    for rt in projects {
        let mut checkouts = vec![rt.primary()];
        checkouts.extend(rt.known.lock().unwrap().values().cloned());
        for c in checkouts {
            let links: Vec<String> = c
                .routes()
                .iter()
                .map(|(svc, host, port)| {
                    let host = html_escape(host);
                    let label = svc
                        .as_deref()
                        .map(|s| format!(" ({})", html_escape(s)))
                        .unwrap_or_default();
                    format!("<a href=\"https://{host}\">{host}</a>{label} :{port}")
                })
                .collect();
            rows.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td><td><code>{}</code></td></tr>",
                html_escape(&rt.project.name),
                html_escape(c.worktree.as_deref().unwrap_or("(primary)")),
                links.join("<br>"),
                html_escape(&c.dev_db()),
                html_escape(&c.path.display().to_string()),
            ));
        }
    }
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>localforest</title>
<style>body{{font:14px system-ui;margin:2em;color:#222;background:#fff}}table{{border-collapse:collapse}}td,th{{padding:.3em .8em;border-bottom:1px solid #ddd;text-align:left}}
@media(prefers-color-scheme:dark){{body{{color:#ddd;background:#111}}a{{color:#8af}}td,th{{border-color:#333}}}}</style></head>
<body><h1>localforest</h1><p>PostgreSQL 127.0.0.1:{} · Redis 127.0.0.1:{} (password = checkout)</p>
<table><tr><th>project</th><th>worktree</th><th>services</th><th>databases</th><th>path</th></tr>{rows}</table></body></html>"#,
        d.global.pg_port, d.global.redis_port
    )
}

// ---------- entry point

/// The PostgreSQL proxy's decision for `user` opening `db`, `c` being the registered
/// checkout whose role `user` is: Err refuses, Ok(true) lets a worktree through to its
/// dev database (created first if missing), Ok(false) lets it through as is. Fails
/// closed: users that are no registered checkout's role never reach the server.
fn pg_access(user: &str, c: Option<&Checkout>, db: &str, others: &[Checkout]) -> Result<bool> {
    if user == "postgres" {
        anyhow::bail!(
            "connect as your checkout's role (PGUSER in `localforest env`), not postgres"
        );
    }
    let Some(c) = c else {
        anyhow::bail!(
            "{user:?} is not the role of a checkout localforest serves; use PGUSER / DATABASE_URL from `localforest env` in the checkout (and `localforest serve` in its project)"
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
        anyhow::bail!(
            "{user} may only open its own databases ({}, {}, MIX_TEST_PARTITION's {}<N>), not {db}",
            c.dev_db(),
            c.test_db(),
            c.test_db()
        );
    }
    Ok(c.worktree.is_some() && db == c.dev_db())
}

async fn daemon_alive() -> bool {
    crate::client::get::<serde_json::Value>("/status")
        .await
        .is_ok()
}

/// `localforest serve`: become the daemon, or register with the running one and take
/// over when it goes away.
pub async fn serve(global: Global, project: Option<Project>) -> Result<()> {
    std::fs::create_dir_all(config::home())?;
    let mut registered = false;
    loop {
        if daemon_alive().await {
            if let Some(p) = &project
                && !registered
            {
                crate::client::post::<serde_json::Value>("/projects", p).await?;
                info!("registered {} with the running localforest daemon", p.name);
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
            Err(e) => {
                debug!("another daemon is starting ({e}); waiting");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
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

async fn lead(global: Global, project: Option<Project>, listener: UnixListener) -> Result<()> {
    info!(
        "localforest daemon {} (state in {})",
        std::process::id(),
        config::home().display()
    );
    let ca = Arc::new(Ca::load_or_create(&config::home().join("ca"))?);
    // The real server: unix socket only, one port above the proxy's.
    let pg = Postgres::start(
        config::pg_dir(),
        global.pg_port + 1,
        global.ramdisk_mb,
        global.postgres_bin.clone(),
        global.postgres_settings()?,
    )
    .await?;
    // Fatal: without it roles from before could still be superusers.
    pg.prepare_templates(&global.postgres_extensions())
        .await
        .context("preparing PostgreSQL")?;
    let activity = history::Activity::default();
    let d = Arc::new(Daemon {
        global: global.clone(),
        pg,
        create_lock: Default::default(),
        dev_locks: Default::default(),
        redis: Arc::new(Redis::new(
            crate::redis::dir(),
            activity.clone(),
            global.redis_server.clone(),
        )),
        servers: Servers::default(),
        routes: Routes::new(activity.clone()),
        activity,
        projects: Mutex::new(BTreeMap::new()),
        shutdown: Notify::new(),
    });

    let weak = Arc::downgrade(&d);
    let resolve: crate::pgproxy::Resolve = Arc::new(move |user, db| {
        let weak = weak.clone();
        Box::pin(async move {
            match weak.upgrade() {
                Some(d) => d.resolve_pg(&user, &db).await,
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
    tokio::spawn(async move {
        if let Err(e) = redis.serve(redis_port).await {
            error!("Redis: {e:#}");
        }
    });
    let weak = Arc::downgrade(&d);
    let dash: proxy::Dashboard =
        Arc::new(move || weak.upgrade().map(|d| dashboard(&d)).unwrap_or_default());
    let weak = Arc::downgrade(&d);
    let ensure: proxy::Ensure = Arc::new(move |host| {
        let weak = weak.clone();
        Box::pin(async move {
            let d = weak.upgrade()?;
            let e = d.ensure_server(&host).await.err()?;
            warn!("{host}: {e:#}");
            Some(format!("{e:#}"))
        })
    });
    let weak = Arc::downgrade(&d);
    let fallback: proxy::Fallback = Arc::new(move |u| {
        let weak = weak.clone();
        Box::pin(async move { weak.upgrade()?.unrouted(u).await })
    });
    let (routes, https, http) = (d.routes.clone(), global.https_port, global.http_port);
    tokio::spawn(async move {
        if let Err(e) = proxy::serve(https, http, routes, ca, dash, ensure, fallback).await {
            error!("HTTPS proxy: {e:#}");
        }
    });
    // Services' `restart` policies.
    let weak = Arc::downgrade(&d);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(d) = weak.upgrade() else { break };
            d.servers.supervise().await;
        }
    });
    // Idle previews.
    let weak = Arc::downgrade(&d);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let Some(d) = weak.upgrade() else { break };
            let projects: Vec<Arc<ProjectRt>> =
                d.projects.lock().unwrap().values().cloned().collect();
            for rt in projects {
                if let Err(e) = d.close_idle_previews(&rt).await {
                    warn!("{}: previews: {e:#}", rt.project.name);
                }
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
    let rts: Vec<_> = std::mem::take(&mut *d.projects.lock().unwrap())
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
mod tests {
    use super::*;

    fn failed(attempts: u32, ago: Duration) -> MigrateFailure {
        MigrateFailure {
            error: String::new(),
            attempts,
            at: std::time::Instant::now().checked_sub(ago).unwrap(),
        }
    }

    fn co(wt: Option<&str>) -> Checkout {
        Checkout {
            project: "demo".into(),
            db_prefix: "demo".into(),
            worktree: wt.map(str::to_string),
            path: "/x".into(),
            port: 20000,
            services: Default::default(),
        }
    }

    #[test]
    fn migrate_backoff_doubles_and_caps() {
        assert!(failed(1, Duration::from_secs(30)).backing_off());
        assert!(!failed(1, Duration::from_secs(61)).backing_off());
        assert!(failed(2, Duration::from_secs(100)).backing_off());
        assert!(!failed(2, Duration::from_secs(121)).backing_off());
        // Capped at 32 minutes.
        assert!(failed(50, Duration::from_secs(31 * 60)).backing_off());
        assert!(!failed(50, Duration::from_secs(33 * 60)).backing_off());
    }

    #[test]
    fn pg_access_fails_closed() {
        let wt = co(Some("wt"));
        // Not a registered checkout's role: refused, whatever the database.
        for db in ["postgres", "demo_dev", "anything"] {
            let e = pg_access("stranger", None, db, &[]).unwrap_err();
            assert!(e.to_string().contains("not the role of a checkout"), "{e}");
        }
        assert!(pg_access("postgres", None, "postgres", &[]).is_err());
        assert!(pg_access("postgres", Some(&co(None)), "postgres", &[]).is_err());
        // Registered: maintenance and own databases, the worktree's dev one created.
        assert!(!pg_access("demo--wt", Some(&wt), "postgres", &[]).unwrap());
        assert!(!pg_access("demo--wt", Some(&wt), "template1", &[]).unwrap());
        assert!(pg_access("demo--wt", Some(&wt), "demo_dev_wt", &[]).unwrap());
        assert!(!pg_access("demo--wt", Some(&wt), "demo_test_wt", &[]).unwrap());
        assert!(!pg_access("demo", Some(&co(None)), "demo_dev", &[]).unwrap());
        assert!(pg_access("demo--wt", Some(&wt), "demo_dev", &[]).is_err());
        // Partitions: a registered worktree's own name wins; others (and itself) listed.
        let (x, x2) = (co(Some("x")), co(Some("x2")));
        let all = [x.clone(), x2.clone()];
        assert!(pg_access("demo--x", Some(&x), "demo_test_x3", &all).is_ok());
        let e = pg_access("demo--x", Some(&x), "demo_test_x2", &all).unwrap_err();
        assert!(e.to_string().contains("demo--x2"), "{e}");
        assert!(pg_access("demo--x2", Some(&x2), "demo_test_x2", &all).is_ok());
        // Cross-project tie: nobody may open it.
        let a = Checkout {
            project: "shop".into(),
            db_prefix: "shop".into(),
            ..co(Some("dev-x"))
        };
        let b = Checkout {
            project: "shop-dev".into(),
            db_prefix: "shop_dev".into(),
            ..co(Some("x"))
        };
        assert_eq!(a.dev_db(), b.dev_db());
        let both = [a.clone(), b.clone()];
        assert!(pg_access("shop-dev-x", Some(&a), "shop_dev_dev_x", &both).is_err());
        assert!(pg_access("shop-dev-x", Some(&b), "shop_dev_dev_x", &both).is_err());
    }
}
