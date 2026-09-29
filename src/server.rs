//! Checkout services (`lazy-cow-tree.services`): each runs its command with the service's
//! env (PORT, hostname, database, Redis) in its own process group. An http service is
//! started by the first request to its https://…localhost hostname that finds nothing
//! listening (or `lazy-cow-tree service start`), after the services it depends on; all of
//! them are killed (whole group, SIGKILL) with the checkout. `restart` restarts one
//! that exits on its own; `restartOnPull` one whose checkout pulled the base branch.
//! `restartOnChange` restarts a running one when the content of files in its `cwd`
//! changes (after the setup command, e.g. `mix deps.get`, when a dependency file did;
//! not while its checkout pulls or migrates); for Mix commands it defaults to what
//! Phoenix's code reloader refuses to compile after until the server restarts.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use notify::{RecursiveMode, Watcher};
use tokio::{net::TcpStream, process::Child, sync::Mutex};
use tracing::{info, warn};

use sha2::Digest;

use crate::config::{self, Checkout, Global, Project, Restart, Service};

/// `cmdline` split like a shell would, run directly (no shell) in `cwd` of the
/// checkout at `checkout` with the registering project's environment (its PATH finds
/// the program), then `env`, then the checkout's env files (`Project::env_files`).
/// In a worktree the project environment was captured in the primary checkout, so
/// its paths into the primary move to the worktree (`config::rewrite_root`: env values,
/// arguments, `cwd`, and a script whose text names the primary runs as a rewritten
/// copy), and DEVENV_ROOT / _DOTFILE / _STATE / _RUNTIME are the worktree's.
/// devenv's own postgres/redis state, exported into the captured project env even
/// though lazy-cow-tree serves them (the module keeps `services.*.enable` readable): a
/// process pointing at the primary's data directory would bypass lazy-cow-tree.
const DEVENV_SERVICE_STATE: &[&str] = &["PGDATA", "REDISDATA"];

pub fn command(
    project: &Project,
    checkout: &Path,
    cmdline: &str,
    cwd: &Path,
    env: Vec<(String, String)>,
) -> Result<tokio::process::Command> {
    let root = project.root.as_path();
    let worktree = checkout != root;
    let rw = |s: &str| {
        if worktree {
            config::rewrite_root(s, root, checkout)
        } else {
            s.to_string()
        }
    };
    let (file_vars, primary_only) = config::env_file_vars(root, checkout, &project.env_files());
    let base: Vec<(String, String)> = project
        .env
        .iter()
        .filter(|(k, _)| !primary_only.contains(k) && !DEVENV_SERVICE_STATE.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), rw(v)))
        .collect();
    let argv = shell_words::split(cmdline)?;
    let (prog, args) = argv.split_first().context("empty command")?;
    let prog = rw(prog);
    let program = base
        .iter()
        .find(|(k, _)| k == "PATH")
        .filter(|_| !prog.contains('/'))
        .and_then(|(_, path)| {
            std::env::split_paths(path)
                .map(|d| d.join(&prog))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| PathBuf::from(&prog));
    let program = if worktree {
        rewritten_script(&program, root, checkout, &config::home().join("scripts"))
            .unwrap_or(program)
    } else {
        program
    };
    let mut c = tokio::process::Command::new(program);
    c.args(args.iter().map(|a| rw(a)))
        .current_dir(rw(&cwd.to_string_lossy()))
        .env_clear()
        .envs(base)
        .envs(env.into_iter().map(|(k, v)| {
            let v = rw(&v);
            (k, v)
        }))
        .stdin(Stdio::null());
    if worktree {
        c.envs(devenv_vars(checkout));
    }
    for (k, _) in &file_vars {
        if config::RESERVED_ENV.contains(&k.as_str()) || k.starts_with("LAZY_COW_TREE_") {
            warn_once(checkout, k);
        }
    }
    c.envs(file_vars);
    Ok(c)
}

/// DEVENV_* of a worktree: its own root, state and a short runtime dir (unix
/// sockets must fit 104 bytes).
pub fn devenv_vars(checkout: &Path) -> Vec<(String, String)> {
    let h = hex::encode(&sha2::Sha256::digest(checkout.to_string_lossy().as_bytes())[..4]);
    let runtime = PathBuf::from("/tmp").join(format!("lazy-cow-tree-{h}"));
    let _ = std::fs::create_dir_all(&runtime);
    let dotfile = checkout.join(".devenv");
    vec![
        ("DEVENV_ROOT".into(), checkout.display().to_string()),
        ("DEVENV_DOTFILE".into(), dotfile.display().to_string()),
        (
            "DEVENV_STATE".into(),
            dotfile.join("state").display().to_string(),
        ),
        ("DEVENV_RUNTIME".into(), runtime.display().to_string()),
    ]
}

/// A script (devenv's `exec` compiled to a store file) whose text names the primary
/// checkout: a copy for `checkout` in `dir` (`<home>/scripts`), content-addressed.
fn rewritten_script(program: &Path, root: &Path, checkout: &Path, dir: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(program).ok()?;
    if !meta.is_file() || meta.len() > 1 << 20 {
        return None;
    }
    let text = String::from_utf8(std::fs::read(program).ok()?).ok()?;
    let new = config::rewrite_root(&text, root, checkout);
    if new == text {
        return None;
    }
    let h = hex::encode(&sha2::Sha256::digest(new.as_bytes())[..8]);
    let name = program.file_name()?.to_string_lossy();
    let path = dir.join(format!("{h}-{name}"));
    if !path.exists() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent()?).ok()?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, &new).ok()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
    }
    Some(path)
}

/// Warn once per checkout and key that an env file overrides a lazy-cow-tree variable.
fn warn_once(checkout: &Path, key: &str) {
    static SEEN: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
    let k = format!("{}\0{key}", checkout.display());
    if SEEN.lock().unwrap().insert(k) {
        warn!(
            "{}: an env file overrides {key}, which lazy-cow-tree sets",
            checkout.display()
        );
    }
}

/// Log of a service (`Checkout::service_id`) or a migrate run.
pub fn log_path(id: &str) -> PathBuf {
    config::home().join(format!("logs/{id}.log"))
}

async fn listening(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).await.is_ok()
}

/// GET `path` on 127.0.0.1:`port` answers 2xx or 3xx (2 s at most).
pub async fn http_ready(port: u16, path: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let probe = async {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .ok()?;
        let mut buf = [0u8; 16];
        let mut n = 0;
        while n < 12 {
            let r = s.read(&mut buf[n..]).await.ok()?;
            if r == 0 {
                break;
            }
            n += r;
        }
        let head = std::str::from_utf8(&buf[..n]).ok()?;
        let code: u16 = head.strip_prefix("HTTP/1.")?.get(2..5)?.parse().ok()?;
        Some((200..400).contains(&code))
    };
    tokio::time::timeout(Duration::from_secs(2), probe)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// A running service and what it takes to start it again.
struct Proc {
    child: Child,
    project: Project,
    checkout: Checkout,
    name: String,
    global: Global,
    started: Instant,
    /// Restarts in a row that didn't stay up a minute (backoff).
    failures: u32,
    /// Exited; start again at this time (per its `restart` policy).
    respawn_at: Option<Instant>,
    /// Its `restartOnChange` files, if any.
    watched: Option<Watched>,
}

impl Proc {
    fn new(project: &Project, c: &Checkout, name: &str, g: &Global, child: Child) -> Self {
        let watched = c
            .service(name)
            .map(restart_patterns)
            .filter(|p| !p.is_empty())
            .and_then(|p| Some(Watched::new(service_cwd(c, name)?, p)));
        Proc {
            child,
            project: project.clone(),
            checkout: c.clone(),
            name: name.to_string(),
            global: g.clone(),
            started: Instant::now(),
            failures: 0,
            respawn_at: None,
            watched,
        }
    }

    /// Runs `child` from now, with the `restartOnChange` files as they are.
    fn started_with(&mut self, child: Child) {
        self.child = child;
        self.started = Instant::now();
        self.respawn_at = None;
        if let Some(w) = &mut self.watched {
            w.files = hashes(&w.dir, &w.patterns);
        }
    }

    fn alive(&mut self) -> bool {
        self.respawn_at.is_none() && matches!(self.child.try_wait(), Ok(None))
    }

    /// Its `restartOnChange` files whose content differs from when it started.
    fn changed_files(&self) -> Vec<PathBuf> {
        let Some(w) = &self.watched else {
            return Vec::new();
        };
        let now = hashes(&w.dir, &w.patterns);
        now.keys()
            .chain(w.files.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|f| now.get(*f) != w.files.get(*f))
            .cloned()
            .collect()
    }
}

/// A service's `restartOnChange` patterns, in its working directory, and the content
/// hashes of the files they matched as it started.
struct Watched {
    dir: PathBuf,
    patterns: Vec<String>,
    /// The patterns' dirs, and the service's own: a pattern's dir made later shows
    /// up there.
    dirs: BTreeSet<PathBuf>,
    files: BTreeMap<PathBuf, u64>,
}

impl Watched {
    fn new(dir: PathBuf, patterns: Vec<String>) -> Self {
        for p in &patterns {
            if Path::new(p)
                .parent()
                .is_some_and(|d| d.to_string_lossy().contains(['*', '?']))
            {
                warn!("restartOnChange `{p}`: wildcards only work in the file name");
            }
        }
        let dirs = patterns
            .iter()
            .map(|p| {
                let d = dir.join(Path::new(p).parent().unwrap_or(Path::new("")));
                d.canonicalize().unwrap_or(d)
            })
            .chain([dir.clone()])
            .collect();
        let files = hashes(&dir, &patterns);
        Watched {
            dir,
            patterns,
            dirs,
            files,
        }
    }

    /// Whether an event on `path` may concern these files (FSEvents reports
    /// canonical paths; the dirs are canonicalized too).
    fn concerns(&self, path: &Path) -> bool {
        self.dirs
            .iter()
            .any(|d| path == d || path.parent() == Some(d))
    }
}

/// What Phoenix's code reloader checks (`Mix.Project.config_files` and the lockfile),
/// plus runtime.exs.
const MIX_FILES: &[&str] = &["mix.exs", "mix.lock", "config/*.exs"];

/// Dependency manifests and lockfiles: the setup command runs before a
/// `restartOnChange` restart only when one of these changed.
const DEPENDENCY_FILES: &[&str] = &[
    "mix.exs",
    "mix.lock",
    "Gemfile",
    "Gemfile.lock",
    "package.json",
    "package-lock.json",
    "bun.lock",
    "bun.lockb",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.toml",
    "Cargo.lock",
    "go.mod",
    "go.sum",
    "pyproject.toml",
    "uv.lock",
    "poetry.lock",
    "requirements.txt",
    "composer.json",
    "composer.lock",
];

fn is_dependency_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| DEPENDENCY_FILES.iter().any(|d| n == *d))
}

/// Runs Mix: `mix phx.server`, `iex -S mix phx.server`, `with-secrets -- mix …`,
/// `sh -c '… && mix phx.server'`. A false positive (Laravel Mix) is harmless: its
/// `MIX_FILES` don't exist, so nothing restarts it.
fn is_mix(exec: &str) -> bool {
    shell_words::split(exec).is_ok_and(|argv| {
        argv.iter()
            .flat_map(|a| a.split_whitespace())
            .any(|a| Path::new(a).file_name().is_some_and(|n| n == "mix"))
    })
}

fn service_cwd(c: &Checkout, name: &str) -> Option<PathBuf> {
    let svc = c.service(name)?;
    let cwd = svc
        .cwd
        .as_ref()
        .map_or_else(|| c.path.clone(), |d| c.path.join(d));
    Some(cwd.canonicalize().unwrap_or(cwd))
}

/// `restartOnChange`, by default `MIX_FILES` for a Mix command.
fn restart_patterns(svc: &Service) -> Vec<String> {
    match &svc.restart_on_change {
        Some(p) => p.clone(),
        None if is_mix(&svc.exec) => MIX_FILES.iter().map(|p| p.to_string()).collect(),
        None => Vec::new(),
    }
}

/// `*` (any run) and `?` (any one) in a file name.
fn wildcard(pattern: &[u8], name: &[u8]) -> bool {
    match (pattern.split_first(), name.split_first()) {
        (None, None) => true,
        (Some((b'*', rest)), _) => {
            wildcard(rest, name) || (!name.is_empty() && wildcard(pattern, &name[1..]))
        }
        (Some((b'?', p)), Some((_, n))) => wildcard(p, n),
        (Some((a, p)), Some((b, n))) => a == b && wildcard(p, n),
        _ => false,
    }
}

/// Content hashes of the files `patterns` (relative to `dir`, wildcards in the file
/// name) match.
fn hashes(dir: &Path, patterns: &[String]) -> BTreeMap<PathBuf, u64> {
    let mut files = BTreeSet::new();
    for pattern in patterns {
        let p = dir.join(pattern);
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
            continue;
        };
        let name = name.as_encoded_bytes();
        if !name.iter().any(|b| matches!(b, b'*' | b'?')) {
            files.insert(p.clone());
            continue;
        }
        for e in std::fs::read_dir(parent).into_iter().flatten().flatten() {
            if wildcard(name, e.file_name().as_encoded_bytes()) {
                files.insert(e.path());
            }
        }
    }
    files
        .into_iter()
        .filter_map(|p| {
            let mut h = DefaultHasher::new();
            std::fs::read(&p).ok()?.hash(&mut h);
            Some((p, h.finish()))
        })
        .collect()
}

fn spawn(project: &Project, c: &Checkout, name: &str, g: &Global, append: bool) -> Result<Child> {
    let svc = c
        .service(name)
        .ok_or_else(|| anyhow!("no service {name}"))?;
    let id = c.service_id(name);
    let log = log_path(&id);
    std::fs::create_dir_all(log.parent().unwrap())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&log)?;
    let cwd = svc
        .cwd
        .as_ref()
        .map_or_else(|| c.path.clone(), |d| c.path.join(d));
    let child = command(
        project,
        &c.path,
        &svc.exec,
        &cwd,
        c.service_env(g, Some(name)),
    )?
    .stdout(log.try_clone()?)
    .stderr(log)
    // Own group: removal kills it and every watcher it started.
    .process_group(0)
    .spawn()
    .with_context(|| format!("starting {id}: `{}`", svc.exec))?;
    info!(
        "{id}: started `{}` (port {})",
        svc.exec,
        c.service_port(name)
    );
    Ok(child)
}

fn kill_group(child: &Child) {
    if let Some(pid) = child.id() {
        unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
    }
}

/// Run a project command (migrate, seed, setup) in `cwd`, logged to `logs/<id>.log`
/// (appended to with `append`); 15 minutes at most.
pub async fn run_logged(
    project: &Project,
    checkout: &Path,
    id: &str,
    cmd: &str,
    cwd: &Path,
    env: Vec<(String, String)>,
    append: bool,
) -> Result<()> {
    let log_path = log_path(id);
    std::fs::create_dir_all(log_path.parent().unwrap())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&log_path)?;
    info!("{id}: running `{cmd}`");
    let t = Instant::now();
    let status = command(project, checkout, cmd, cwd, env)?
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .status();
    let status = tokio::time::timeout(Duration::from_secs(900), status)
        .await
        .map_err(|_| anyhow!("`{cmd}` timed out"))??;
    if !status.success() {
        bail!(
            "`{cmd}` failed in {id} ({status}); see {}",
            log_path.display()
        );
    }
    info!("{id}: done in {:?}", t.elapsed());
    Ok(())
}

/// The project's setup command (`mix deps.get`) in a checkout whose dependency files
/// changed, before restarting its services; appended to the log of its first run. A
/// failure is only logged: the dependencies may be there already.
async fn run_setup(project: &Project, c: &Checkout, g: &Global) {
    let Some(cmd) = &project.settings.setup else {
        return;
    };
    let id = c.run_id("setup");
    if let Err(e) = run_logged(project, &c.path, &id, cmd, &c.path, c.env(g), true).await {
        warn!("{e:#}");
    }
}

/// Kill a running service's group and start it again.
async fn respawn(id: &str, p: &mut Proc) {
    kill_group(&p.child);
    let _ = p.child.wait().await;
    match spawn(&p.project, &p.checkout, &p.name, &p.global, true) {
        Ok(child) => {
            p.started_with(child);
            p.failures = 0;
            info!("{id}: restarted");
        }
        Err(e) => warn!("{e:#}"),
    }
}

/// The notify watcher and the dirs it watches (or failed to).
#[derive(Default)]
struct Watches {
    watcher: Option<notify::RecommendedWatcher>,
    dirs: HashSet<PathBuf>,
}

#[derive(Default)]
pub struct Servers {
    /// By service id.
    procs: Mutex<HashMap<String, Proc>>,
    /// The dirs of running services' `restartOnChange` files.
    watches: std::sync::Mutex<Watches>,
    /// Changed paths by their last change (a checkout's restarts wait for a second
    /// of quiet: `mix deps.get` and `git merge` write several files).
    changed: Arc<std::sync::Mutex<HashMap<PathBuf, Instant>>>,
    /// Checkouts (and those under them) whose services aren't restarted for changed
    /// files now, by holders: pulling / migrating, or running setup. Changes wait.
    held: std::sync::Mutex<HashMap<PathBuf, usize>>,
}

/// Holds back `restartOnChange` restarts of a checkout (and those under it) until
/// dropped (`Servers::hold`).
pub struct Hold {
    servers: Arc<Servers>,
    path: PathBuf,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut held = self.servers.held.lock().unwrap();
        if let Some(n) = held.get_mut(&self.path) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.path);
            }
        }
    }
}

impl Servers {
    /// Hold back `restartOnChange` restarts in `path` (a checkout, or the primary
    /// for all of a project's) while pulling or migrating: they run once released, and
    /// not at all for services `restart`ed meanwhile.
    pub fn hold(self: &Arc<Self>, path: &Path) -> Hold {
        *self
            .held
            .lock()
            .unwrap()
            .entry(path.to_path_buf())
            .or_default() += 1;
        Hold {
            servers: self.clone(),
            path: path.to_path_buf(),
        }
    }

    fn held(&self, checkout: &Path) -> bool {
        self.held
            .lock()
            .unwrap()
            .keys()
            .any(|h| checkout.starts_with(h))
    }

    /// Watch the dirs of the `restartOnChange` files of `procs`, and only those.
    fn sync_watches(&self, procs: &HashMap<String, Proc>) {
        let want: HashSet<PathBuf> = procs
            .values()
            .filter_map(|p| p.watched.as_ref())
            .flat_map(|w| w.dirs.iter().cloned())
            .filter(|d| d.is_dir())
            .collect();
        let mut watches = self.watches.lock().unwrap();
        if watches.dirs == want {
            return;
        }
        if watches.watcher.is_none() {
            let changed = self.changed.clone();
            let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(ev) = res else { return };
                let now = Instant::now();
                let mut changed = changed.lock().unwrap();
                for p in ev.paths {
                    changed.insert(p, now);
                }
            });
            match w {
                Ok(w) => watches.watcher = Some(w),
                Err(e) => {
                    // Tried again only once the dirs change.
                    warn!("watching restartOnChange files: {e}");
                    watches.dirs = want;
                    return;
                }
            }
        }
        let Watches { watcher, dirs } = &mut *watches;
        let w = watcher.as_mut().unwrap();
        for d in dirs.difference(&want) {
            let _ = w.unwatch(d);
        }
        for d in want.difference(dirs) {
            if let Err(e) = w.watch(d, RecursiveMode::NonRecursive) {
                warn!("watching {}: {e}", d.display());
            }
        }
        *dirs = want;
    }

    /// Restart running services whose `restartOnChange` files changed, per checkout
    /// once nothing in it changed for a second and it isn't `held`. When a dependency
    /// file changed, the project's setup command (`mix deps.get`) runs first, in the
    /// background (the checkout held meanwhile).
    async fn restart_changed(self: &Arc<Self>) {
        let (events, at) = {
            let changed = self.changed.lock().unwrap();
            if changed.is_empty() {
                return;
            }
            (changed.clone(), Instant::now())
        };
        {
            // A watched dir itself changed (removed, replaced): inotify dropped its
            // watch; `sync_watches` watches it again.
            let mut watches = self.watches.lock().unwrap();
            let Watches { watcher, dirs } = &mut *watches;
            for p in events.keys() {
                if dirs.remove(p)
                    && let Some(w) = watcher.as_mut()
                {
                    let _ = w.unwatch(p);
                }
            }
        }
        let mut procs = self.procs.lock().await;
        let mut checkouts: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        for (id, p) in procs.iter() {
            if p.watched.is_some() {
                checkouts
                    .entry(p.checkout.path.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        let mut keep = HashSet::new();
        for (path, ids) in checkouts {
            let concerned: Vec<&PathBuf> = events
                .keys()
                .filter(|e| {
                    ids.iter()
                        .any(|id| procs[id].watched.as_ref().is_some_and(|w| w.concerns(e)))
                })
                .collect();
            if concerned.is_empty() {
                continue;
            }
            let quiet = concerned
                .iter()
                .all(|e| events[*e].elapsed() >= Duration::from_secs(1));
            if !quiet || self.held(&path) {
                keep.extend(concerned.into_iter().cloned());
                continue;
            }
            let mut restart = Vec::new();
            let mut deps = false;
            for id in ids {
                let p = procs.get_mut(&id).unwrap();
                if !p.alive() {
                    continue;
                }
                let changed = p.changed_files();
                if changed.is_empty() {
                    continue;
                }
                let dir = &p.watched.as_ref().unwrap().dir;
                let what: Vec<String> = changed
                    .iter()
                    .map(|f| f.strip_prefix(dir).unwrap_or(f).display().to_string())
                    .collect();
                info!("{id}: {} changed; restarting", what.join(", "));
                deps |= changed.iter().any(|f| is_dependency_file(f));
                restart.push((id, p.started));
            }
            let Some((first, _)) = restart.first() else {
                continue;
            };
            let p = &procs[first];
            if !deps || p.project.settings.setup.is_none() {
                for (id, _) in restart {
                    respawn(&id, procs.get_mut(&id).unwrap()).await;
                }
                continue;
            }
            let (project, c, g) = (p.project.clone(), p.checkout.clone(), p.global.clone());
            let hold = self.hold(&path);
            let servers = self.clone();
            tokio::spawn(async move {
                run_setup(&project, &c, &g).await;
                let mut procs = servers.procs.lock().await;
                for (id, started) in restart {
                    // Not one stopped and started again meanwhile.
                    if let Some(p) = procs.get_mut(&id)
                        && p.started == started
                        && p.alive()
                    {
                        respawn(&id, p).await;
                    }
                }
                drop(hold);
            });
        }
        drop(procs);
        self.changed
            .lock()
            .unwrap()
            .retain(|p, t| keep.contains(p) || *t >= at);
    }

    /// Start `name` and (first) everything it depends on; http services are waited
    /// for until they listen (3 minutes: first compiles are slow).
    pub async fn ensure(
        &self,
        project: &Project,
        c: &Checkout,
        name: &str,
        g: &Global,
    ) -> Result<()> {
        let mut order = Vec::new();
        visit(c, name, &mut BTreeSet::new(), &mut order)?;
        for n in order {
            self.ensure_one(project, c, &n, g).await?;
        }
        Ok(())
    }

    /// `ensure` the service owning `port`, then wait for `port` too when it is one of
    /// its secondary ports: 5 s once its main port listens (bound a moment after it),
    /// 3 minutes for a service without http (nothing tells it is up).
    pub async fn ensure_port(
        &self,
        project: &Project,
        c: &Checkout,
        name: &str,
        port: u16,
        g: &Global,
    ) -> Result<()> {
        self.ensure(project, c, name, g).await?;
        if port == c.service_port(name) {
            return Ok(());
        }
        let id = c.service_id(name);
        let port_name = c
            .port_slots()
            .into_iter()
            .find(|p| p.service == name && c.port + p.offset == port)
            .map_or_else(|| "a".to_string(), |p| p.name);
        let http = c.service(name).is_some_and(|s| s.http);
        let tries = if http { 50 } else { 1800 };
        for _ in 0..tries {
            if listening(port).await {
                return Ok(());
            }
            if !self.running(&id).await {
                bail!("{id} exited; see {}", log_path(&id).display());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!(
            "{name} doesn't listen on its {port_name} port {port}; see {}",
            log_path(&id).display()
        )
    }

    async fn ensure_one(
        &self,
        project: &Project,
        c: &Checkout,
        name: &str,
        g: &Global,
    ) -> Result<()> {
        let svc = c
            .service(name)
            .ok_or_else(|| anyhow!("no service {name}"))?;
        let port = c.service_port(name);
        let id = c.service_id(name);
        if svc.http && listening(port).await {
            return Ok(());
        }
        {
            let mut procs = self.procs.lock().await;
            let alive = match procs.get_mut(&id) {
                Some(p) => p.child.try_wait()?.is_none(),
                None => false,
            };
            if !alive {
                let child = spawn(project, c, name, g, false)?;
                procs.insert(id.clone(), Proc::new(project, c, name, g, child));
                self.sync_watches(&procs);
            }
        }
        if !svc.http {
            return Ok(());
        }
        // Its `ready` probe, else listening; 3 minutes (first compiles are slow).
        let limit = svc
            .ready
            .as_ref()
            .map_or(Duration::from_secs(180), |r| Duration::from_secs(r.timeout));
        let t = Instant::now();
        while t.elapsed() < limit {
            let up = match &svc.ready {
                Some(r) => http_ready(port, &r.path).await,
                None => listening(port).await,
            };
            if up {
                return Ok(());
            }
            if let Some(p) = self.procs.lock().await.get_mut(&id)
                && let Some(status) = p.child.try_wait()?
                && svc.restart == Restart::No
            {
                bail!("{id} exited ({status}); see {}", log_path(&id).display());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        match &svc.ready {
            Some(r) => bail!(
                "{id} was not ready (GET {} on port {port}) within {} s",
                r.path,
                r.timeout
            ),
            None => bail!("{id} did not listen on port {port} within 3 minutes"),
        }
    }

    /// Running services with an `idleTimeout`: (id, checkout, name, started, timeout).
    pub async fn idle_candidates(&self) -> Vec<(String, Checkout, String, Instant, Duration)> {
        let mut procs = self.procs.lock().await;
        procs
            .iter_mut()
            .filter_map(|(id, p)| {
                let t = p.checkout.service(&p.name)?.idle_timeout?;
                p.alive().then(|| {
                    (
                        id.clone(),
                        p.checkout.clone(),
                        p.name.clone(),
                        p.started,
                        Duration::from_secs(t),
                    )
                })
            })
            .collect()
    }

    /// Apply `restart` policies: called every second. An exited service with
    /// `on-failure` (non-zero exit) or `always` is started again after a backoff that
    /// doubles per quick exit (1 s .. 30 s) and resets once it stayed up a minute.
    /// Services whose `restartOnChange` files changed are restarted.
    pub async fn supervise(self: &Arc<Self>) {
        self.restart_changed().await;
        let mut procs = self.procs.lock().await;
        let now = Instant::now();
        let mut gone = Vec::new();
        for (id, p) in procs.iter_mut() {
            if let Some(at) = p.respawn_at {
                if now < at {
                    continue;
                }
                match spawn(&p.project, &p.checkout, &p.name, &p.global, true) {
                    Ok(child) => p.started_with(child),
                    Err(e) => {
                        warn!("{e:#}");
                        p.respawn_at = Some(now + Duration::from_secs(30));
                    }
                }
                continue;
            }
            let Ok(Some(status)) = p.child.try_wait() else {
                continue;
            };
            let policy = p
                .checkout
                .service(&p.name)
                .map_or(Restart::No, |s| s.restart);
            let again = match policy {
                Restart::No => false,
                Restart::OnFailure => !status.success(),
                Restart::Always => true,
            };
            if !again {
                info!("{id} exited ({status})");
                gone.push(id.clone());
                continue;
            }
            // Leftovers of its group (watchers) would hold the port.
            kill_group(&p.child);
            if now.duration_since(p.started) > Duration::from_secs(60) {
                p.failures = 0;
            }
            let delay = Duration::from_secs((1u64 << p.failures.min(5)).min(30));
            p.failures += 1;
            warn!("{id} exited ({status}); restarting in {delay:?}");
            p.respawn_at = Some(now + delay);
        }
        for id in gone {
            procs.remove(&id);
        }
        self.sync_watches(&procs);
    }

    /// Kill and start again a running service (after a pull), after the setup command
    /// when its `restartOnChange` dependency files changed; their change then doesn't
    /// restart it again (`restart_changed`).
    pub async fn restart(&self, id: &str) {
        let setup = {
            let procs = self.procs.lock().await;
            let Some(p) = procs.get(id) else { return };
            (p.project.settings.setup.is_some()
                && p.changed_files().iter().any(|f| is_dependency_file(f)))
            .then(|| (p.project.clone(), p.checkout.clone(), p.global.clone()))
        };
        if let Some((project, c, g)) = setup {
            run_setup(&project, &c, &g).await;
        }
        let mut procs = self.procs.lock().await;
        let Some(p) = procs.get_mut(id) else { return };
        respawn(id, p).await;
    }

    pub async fn running(&self, id: &str) -> bool {
        match self.procs.lock().await.get_mut(id) {
            Some(p) => p.respawn_at.is_some() || matches!(p.child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// SIGKILL a service's whole process group; it stays down.
    pub async fn stop(&self, id: &str) {
        if let Some(mut p) = self.procs.lock().await.remove(id) {
            kill_group(&p.child);
            let _ = p.child.wait().await;
            info!("{id}: stopped");
        }
    }

    /// Stop every service of a checkout.
    pub async fn stop_checkout(&self, c: &Checkout) {
        for name in c.services.0.keys() {
            self.stop(&c.service_id(name)).await;
        }
    }

    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.procs.lock().await.keys().cloned().collect();
        for id in ids {
            self.stop(&id).await;
        }
    }
}

/// Dependencies first (depth first), each once; a cycle is an error.
fn visit(
    c: &Checkout,
    name: &str,
    path: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) -> Result<()> {
    if order.iter().any(|n| n == name) {
        return Ok(());
    }
    if !path.insert(name.to_string()) {
        bail!("services depend on each other in a cycle through {name}");
    }
    let svc = c
        .service(name)
        .ok_or_else(|| anyhow!("no service {name}"))?;
    for d in &svc.depends_on {
        visit(c, d, path, order)?;
    }
    path.remove(name);
    order.push(name.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Services;

    /// A one-shot HTTP server answering every connection with `status`.
    async fn answer(status: u16) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 256];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\n\r\n").as_bytes(),
                    )
                    .await;
            }
        });
        port
    }

    #[tokio::test]
    async fn ready_probe() {
        assert!(http_ready(answer(200).await, "/readyz").await);
        assert!(http_ready(answer(302).await, "/").await);
        assert!(!http_ready(answer(503).await, "/").await);
        // Nothing listening.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(!http_ready(port, "/").await);
    }

    #[test]
    fn scripts_naming_the_primary_run_as_rewritten_copies() {
        let d = tempfile::tempdir().unwrap();
        let (root, wt) = (
            Path::new("/src/app"),
            Path::new("/src/app/.claude/worktrees/wt"),
        );
        let script = d.path().join("lazy-cow-tree-web");
        std::fs::write(&script, "#!/bin/sh\ncd /src/app/api\nexec mix phx.server\n").unwrap();
        let out = d.path().join("scripts");
        let copy = rewritten_script(&script, root, wt, &out).unwrap();
        assert!(copy.starts_with(&out));
        assert_eq!(
            std::fs::read_to_string(&copy).unwrap(),
            "#!/bin/sh\ncd /src/app/.claude/worktrees/wt/api\nexec mix phx.server\n"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&copy).unwrap().permissions().mode() & 0o111,
            0o111
        );
        // Same content: same copy.
        assert_eq!(rewritten_script(&script, root, wt, &out), Some(copy));
        // Nothing to rewrite: runs as is.
        std::fs::write(&script, "#!/bin/sh\nexec true\n").unwrap();
        assert_eq!(rewritten_script(&script, root, wt, &out), None);
    }

    #[test]
    fn worktree_commands_get_its_paths_devenv_vars_and_env_files() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("app");
        let wt = root.join(".claude/worktrees/wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(root.join(".env.local"), "FROM_PRIMARY=1\nSHARED=primary\n").unwrap();
        std::fs::write(wt.join(".env.local"), "SHARED=wt\nPORT=1\n").unwrap();
        let mut project = Project::new(
            root.clone(),
            serde_json::from_value(serde_json::json!({
                "name": "app", "port": 4000, "remote": "origin", "base": null,
                "worktrees_dir": ".claude/worktrees",
                "migrate": null, "seed": null, "setup": null, "services": {},
                "preview_ttl_hours": 48, "no_sync": false, "no_auto_remove": false
            }))
            .unwrap(),
        );
        let r = root.display().to_string();
        project.env = vec![
            ("PATH".into(), format!("{r}/bin:/usr/bin:/bin")),
            ("DEVENV_ROOT".into(), r.clone()),
            ("FROM_PRIMARY".into(), "1".into()),
            ("LAZY_COW_TREE_ENV_FILES".into(), r#"[".env.local"]"#.into()),
            ("PGDATA".into(), format!("{r}/.devenv/state/postgres")),
            ("REDISDATA".into(), format!("{r}/.devenv/state/redis")),
        ];
        let cmd = command(
            &project,
            &wt,
            "echo hi",
            &root.join("api"),
            vec![
                ("PORT".into(), "20000".into()),
                ("CFG".into(), format!("{r}/config")),
            ],
        )
        .unwrap();
        let std = cmd.as_std();
        let env: BTreeMap<String, String> = std
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        let w = wt.display().to_string();
        assert_eq!(std.get_current_dir(), Some(wt.join("api").as_path()));
        assert_eq!(env["PATH"], format!("{w}/bin:/usr/bin:/bin"));
        assert_eq!(env["DEVENV_ROOT"], w);
        assert_eq!(env["DEVENV_STATE"], format!("{w}/.devenv/state"));
        assert!(env["DEVENV_RUNTIME"].starts_with("/tmp/lazy-cow-tree-"));
        assert_eq!(env["CFG"], format!("{w}/config"));
        // Env files come last: over the derived PORT too.
        assert_eq!(env["SHARED"], "wt");
        assert_eq!(env["PORT"], "1");
        // Only the primary's file set it: gone in the worktree.
        assert!(!env.contains_key("FROM_PRIMARY"));
        // devenv's own postgres/redis state never reaches what lazy-cow-tree runs.
        assert!(!env.contains_key("PGDATA") && !env.contains_key("REDISDATA"));

        // The primary keeps its paths and its own file's values.
        let cmd = command(&project, &root, "echo hi", &root, vec![]).unwrap();
        let env: BTreeMap<String, String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        assert_eq!(env["DEVENV_ROOT"], r);
        assert_eq!(env["SHARED"], "primary");
        assert_eq!(env["FROM_PRIMARY"], "1");
        let _ = std::fs::remove_dir(&devenv_vars(&wt)[3].1);
    }

    #[test]
    fn dependencies_first() {
        let c = Checkout {
            project: "p".into(),
            db_prefix: "p".into(),
            worktree: None,
            path: "/x".into(),
            port: 4000,
            services: r#"{"web": {"exec": "a", "dependsOn": ["worker", "api"]},
                          "api": {"exec": "b", "dependsOn": ["worker"]},
                          "worker": {"exec": "c", "http": false}}"#
                .parse::<Services>()
                .unwrap(),
        };
        let mut order = Vec::new();
        visit(&c, "web", &mut BTreeSet::new(), &mut order).unwrap();
        assert_eq!(order, ["worker", "api", "web"]);

        let cyclic = Checkout {
            services: r#"{"a": {"exec": "x", "dependsOn": ["b"]}, "b": {"exec": "y", "dependsOn": ["a"]}}"#
                .parse()
                .unwrap(),
            ..c
        };
        assert!(visit(&cyclic, "a", &mut BTreeSet::new(), &mut Vec::new()).is_err());
    }

    #[test]
    fn restart_on_change_defaults() {
        assert!(is_mix("mix phx.server"));
        assert!(is_mix("iex -S mix phx.server"));
        assert!(is_mix("with-secrets -- /nix/store/x/bin/mix run --no-halt"));
        assert!(!is_mix("bun run dev"));
        assert!(is_mix("sh -c 'mix assets.build && mix phx.server'"));
        assert!(!is_mix("mixer serve"));
        assert!(is_dependency_file(Path::new("/a/mix.lock")));
        assert!(is_dependency_file(Path::new("/a/Gemfile.lock")));
        assert!(!is_dependency_file(Path::new("/a/config/dev.exs")));

        let services: Services = r#"{"web": {"exec": "mix phx.server"},
                                     "off": {"exec": "mix phx.server", "restartOnChange": []},
                                     "rails": {"exec": "bin/rails s", "restartOnChange": ["Gemfile.lock"]},
                                     "vite": {"exec": "bun run dev"}}"#
            .parse()
            .unwrap();
        let patterns = |n: &str| restart_patterns(&services.0[n]);
        assert_eq!(patterns("web"), MIX_FILES);
        assert!(patterns("off").is_empty());
        assert_eq!(patterns("rails"), ["Gemfile.lock"]);
        assert!(patterns("vite").is_empty());
    }

    #[test]
    fn holds() {
        let servers = Arc::new(Servers::default());
        let wt = Path::new("/p/.claude/worktrees/x");
        let root = servers.hold(Path::new("/p"));
        let own = servers.hold(wt);
        assert!(servers.held(wt) && servers.held(Path::new("/p")));
        drop(root);
        assert!(servers.held(wt) && !servers.held(Path::new("/p")));
        let again = servers.hold(wt);
        drop(own);
        assert!(servers.held(wt));
        drop(again);
        assert!(!servers.held(wt) && servers.held.lock().unwrap().is_empty());
    }

    #[test]
    fn wildcards() {
        assert!(wildcard(b"*.exs", b"dev.exs"));
        assert!(wildcard(b"*", b""));
        assert!(wildcard(b"?ev.*s", b"dev.exs"));
        assert!(!wildcard(b"*.exs", b"dev.ex"));
        assert!(!wildcard(b"mix.lock", b"mix.lockx"));
    }

    #[test]
    fn watched_files() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().canonicalize().unwrap();
        std::fs::create_dir(dir.join("config")).unwrap();
        for f in [
            "mix.exs",
            "mix.lock",
            "config/dev.exs",
            "config/notes.txt",
            "README.md",
        ] {
            std::fs::write(dir.join(f), "a").unwrap();
        }
        let patterns: Vec<String> = MIX_FILES.iter().map(|p| p.to_string()).collect();
        let w = Watched::new(dir.clone(), patterns.clone());
        assert_eq!(w.files.len(), 3);
        // Rewritten with the same content: nothing to restart for.
        std::fs::write(dir.join("mix.lock"), "a").unwrap();
        assert_eq!(hashes(&dir, &patterns), w.files);
        std::fs::write(dir.join("mix.lock"), "b").unwrap();
        assert_ne!(hashes(&dir, &patterns), w.files);

        assert!(w.concerns(&dir.join("mix.lock")));
        assert!(w.concerns(&dir.join("config/runtime.exs")));
        // Made after the watch began.
        assert!(w.concerns(&dir.join("config")));
        assert!(!w.concerns(&dir.join("lib/a.exs")));
    }
}
