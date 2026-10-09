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
/// the program), then `env`.
/// In a worktree the project environment was captured in the primary checkout, so
/// its paths into the primary move to the worktree (`config::rewrite_root`: env values,
/// arguments, `cwd`, and a script whose text names the primary runs as a rewritten
/// copy), and DEVENV_ROOT / _DOTFILE / _STATE / _RUNTIME are the worktree's.
/// devenv's own postgres/redis state, exported into the captured project env even
/// though lazy-cow-tree serves them (the module keeps `services.*.enable` readable): a
/// process pointing at the primary's data directory would bypass lazy-cow-tree.
const DEVENV_SERVICE_STATE: &[&str] = &["PGDATA", "REDISDATA"];

/// The shell hook's variables in the captured project env. A service's bash would
/// source the hook (BASH_ENV) and take the shell's view of the checkout (PORT of the
/// default service); the user's own BASH_ENV / ZDOTDIR, kept aside by the hook, come back.
fn without_shell_hook(env: Vec<(String, String)>) -> Vec<(String, String)> {
    let saved = |k: &str| {
        env.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let (bash_env, zdotdir) = (
        saved("LAZY_COW_TREE_BASH_ENV"),
        saved("LAZY_COW_TREE_ZDOTDIR"),
    );
    let hook = [
        "BASH_ENV",
        "ZDOTDIR",
        "LAZY_COW_TREE_BASH_ENV",
        "LAZY_COW_TREE_ZDOTDIR",
        "LAZY_COW_TREE_SHELL",
    ];
    env.into_iter()
        .filter(|(k, _)| !hook.contains(&k.as_str()))
        .chain(bash_env.map(|v| ("BASH_ENV".to_string(), v)))
        .chain(zdotdir.map(|v| ("ZDOTDIR".to_string(), v)))
        .collect()
}

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
    let base: Vec<(String, String)> = without_shell_hook(
        project
            .env
            .iter()
            .filter(|(k, _)| !DEVENV_SERVICE_STATE.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), rw(v)))
            .collect(),
    );
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
    Ok(c)
}

/// DEVENV_* of a worktree: its own root, state and a short runtime dir (unix
/// sockets must fit 104 bytes).
pub fn devenv_vars(checkout: &Path) -> Vec<(String, String)> {
    let runtime = config::runtime_dir(checkout);
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
    let mut env = c.service_env(g, Some(name));
    // `lazy-cow-tree service env` wins over the rest.
    env.extend(config::env_overrides(&c.path, name));
    let child = command(project, &c.path, &svc.exec, &cwd, env)?
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
    let status = match tokio::time::timeout(Duration::from_secs(900), status).await {
        Ok(status) => status?,
        Err(_) => bail!("{}", failed(cmd, "timed out", cwd, &log_path)),
    };
    if !status.success() {
        bail!("{}", failed(cmd, &status.to_string(), cwd, &log_path));
    }
    info!("{id}: done in {:?}", t.elapsed());
    Ok(())
}

/// Lines of a failed run's log its error carries (the 502 page, `status`).
const LOG_TAIL_LINES: usize = 40;

/// A failed run's error: the command, where it ran, and the end of its log.
fn failed(cmd: &str, why: &str, cwd: &Path, log_path: &Path) -> String {
    let log = std::fs::read(log_path).unwrap_or_default();
    let log = String::from_utf8_lossy(&log);
    let lines: Vec<&str> = log.lines().collect();
    let tail = lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n");
    format!(
        "`{cmd}` failed ({why})\n  in: {}\n  log: {}\n{tail}",
        cwd.display(),
        log_path.display()
    )
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
    watches: parking_lot::Mutex<Watches>,
    /// Changed paths by their last change (a checkout's restarts wait for a second
    /// of quiet: `mix deps.get` and `git merge` write several files).
    changed: Arc<parking_lot::Mutex<HashMap<PathBuf, Instant>>>,
    /// Checkouts (and those under them) whose services aren't restarted for changed
    /// files now, by holders: pulling / migrating, or running setup. Changes wait.
    held: parking_lot::Mutex<HashMap<PathBuf, usize>>,
    /// Services `lazy-cow-tree service stop` stopped: not started again (by a request,
    /// as a dependency, at `up`) until `service start`, so a foreground run can take
    /// their port.
    stopped: parking_lot::Mutex<HashSet<String>>,
}

/// Holds back `restartOnChange` restarts of a checkout (and those under it) until
/// dropped (`Servers::hold`).
pub struct Hold {
    servers: Arc<Servers>,
    path: PathBuf,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut held = self.servers.held.lock();
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
        *self.held.lock().entry(path.to_path_buf()).or_default() += 1;
        Hold {
            servers: self.clone(),
            path: path.to_path_buf(),
        }
    }

    fn held(&self, checkout: &Path) -> bool {
        self.held.lock().keys().any(|h| checkout.starts_with(h))
    }

    /// Watch the dirs of the `restartOnChange` files of `procs`, and only those.
    fn sync_watches(&self, procs: &HashMap<String, Proc>) {
        let want: HashSet<PathBuf> = procs
            .values()
            .filter_map(|p| p.watched.as_ref())
            .flat_map(|w| w.dirs.iter().cloned())
            .filter(|d| d.is_dir())
            .collect();
        let mut watches = self.watches.lock();
        if watches.dirs == want {
            return;
        }
        if watches.watcher.is_none() {
            let changed = self.changed.clone();
            let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(ev) = res else { return };
                let now = Instant::now();
                let mut changed = changed.lock();
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
            let changed = self.changed.lock();
            if changed.is_empty() {
                return;
            }
            (changed.clone(), Instant::now())
        };
        {
            // A watched dir itself changed (removed, replaced): inotify dropped its
            // watch; `sync_watches` watches it again.
            let mut watches = self.watches.lock();
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
        if self.stopped(&id) {
            bail!(
                "{id} was stopped by `lazy-cow-tree service stop`; run it on port {port} \
                 yourself or `lazy-cow-tree service start -s {name}` in {}",
                c.path.display()
            );
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

    /// `stop`, and keep it down until `release`d.
    pub async fn stop_held(&self, id: &str) {
        self.stopped.lock().insert(id.to_string());
        self.stop(id).await;
    }

    /// Let a `stop_held` service start again.
    pub fn release(&self, id: &str) {
        self.stopped.lock().remove(id);
    }

    pub fn stopped(&self, id: &str) -> bool {
        self.stopped.lock().contains(id)
    }

    /// Stop every service of a checkout (removed: a new one of its name starts fresh).
    pub async fn stop_checkout(&self, c: &Checkout) {
        for name in c.services.0.keys() {
            let id = c.service_id(name);
            self.release(&id);
            self.stop(&id).await;
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
mod tests;
