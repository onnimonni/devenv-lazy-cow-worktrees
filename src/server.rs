//! Checkout services (`localforest.services`): each runs its command with the service's
//! env (PORT, hostname, database, Redis) in its own process group. An http service is
//! started by the first request to its https://…localhost hostname that finds nothing
//! listening (or `localforest service start`), after the services it depends on; all of
//! them are killed (whole group, SIGKILL) with the checkout. `restart` restarts one
//! that exits on its own; `restartOnPull` one whose checkout pulled the base branch.
//! A Mix service is restarted when its `mix.exs`, `mix.lock` or `config/*.exs` change
//! (after the setup command, `mix deps.get`, for `mix.exs` / `mix.lock`): Phoenix's
//! code reloader refuses to compile after that until the server restarts.
//! `restartOnMixChange = false` opts out.

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

use crate::config::{self, Checkout, Global, Project, Restart};

/// `cmdline` split like a shell would, run directly (no shell) in `cwd` with the
/// registering project's environment (its PATH finds the program) plus `env`.
pub fn command(
    project: &Project,
    cmdline: &str,
    cwd: &Path,
    env: Vec<(String, String)>,
) -> Result<tokio::process::Command> {
    let argv = shell_words::split(cmdline)?;
    let (prog, args) = argv.split_first().context("empty command")?;
    let program = project
        .env
        .iter()
        .find(|(k, _)| k == "PATH")
        .filter(|_| !prog.contains('/'))
        .and_then(|(_, path)| {
            std::env::split_paths(path)
                .map(|d| d.join(prog))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| PathBuf::from(prog));
    let mut c = tokio::process::Command::new(program);
    c.args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(project.env.iter().cloned())
        .envs(env)
        .stdin(Stdio::null());
    Ok(c)
}

/// Log of a service (`Checkout::service_id`) or a migrate run.
pub fn log_path(id: &str) -> PathBuf {
    config::home().join(format!("logs/{id}.log"))
}

async fn listening(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).await.is_ok()
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
    /// A Mix service's working directory and its project files as it started.
    mix: Option<(PathBuf, MixFiles)>,
}

/// Content hashes of `mix.exs`, `mix.lock` and `config/*.exs` in a Mix project dir.
type MixFiles = BTreeMap<PathBuf, u64>;

/// Runs Mix: `mix phx.server`, `iex -S mix phx.server`, `with-secrets -- mix …`.
fn is_mix(exec: &str) -> bool {
    shell_words::split(exec).is_ok_and(|argv| {
        argv.iter()
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

/// What Phoenix checks (`Mix.Project.config_files` and the lockfile) plus runtime.exs.
fn mix_files(dir: &Path) -> MixFiles {
    let config = std::fs::read_dir(dir.join("config"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "exs"));
    [dir.join("mix.exs"), dir.join("mix.lock")]
        .into_iter()
        .chain(config)
        .filter_map(|p| {
            let mut h = DefaultHasher::new();
            std::fs::read(&p).ok()?.hash(&mut h);
            Some((p, h.finish()))
        })
        .collect()
}

/// The Mix project dir an event on `path` concerns, if any.
fn mix_dir(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let parent = path.parent()?;
    // `config/` itself: made or removed after the watch began.
    if name == "mix.exs" || name == "mix.lock" || name == "config" {
        Some(parent.to_path_buf())
    } else if path.extension().is_some_and(|e| e == "exs")
        && parent.file_name().is_some_and(|n| n == "config")
    {
        parent.parent().map(Path::to_path_buf)
    } else {
        None
    }
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
    let child = command(project, &svc.exec, &cwd, c.service_env(g, Some(name)))?
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

#[derive(Default)]
pub struct Servers {
    /// By service id.
    procs: Mutex<HashMap<String, Proc>>,
    /// Watches running Mix services' project dirs (and their `config/`).
    watcher: std::sync::Mutex<Option<notify::RecommendedWatcher>>,
    watched: std::sync::Mutex<HashSet<PathBuf>>,
    /// Mix project dirs with changed files, by the last change (restarts wait a second
    /// of quiet: `mix deps.get` and `git merge` write several files).
    changed: Arc<std::sync::Mutex<HashMap<PathBuf, Instant>>>,
}

fn new_proc(project: &Project, c: &Checkout, name: &str, g: &Global, child: Child) -> Proc {
    let mix = c
        .service(name)
        .filter(|s| s.restart_on_mix_change && is_mix(&s.exec))
        .and_then(|_| service_cwd(c, name))
        .map(|dir| {
            let files = mix_files(&dir);
            (dir, files)
        });
    Proc {
        child,
        project: project.clone(),
        checkout: c.clone(),
        name: name.to_string(),
        global: g.clone(),
        started: Instant::now(),
        failures: 0,
        respawn_at: None,
        mix,
    }
}

/// The project's setup command (`mix deps.get`) in the checkout, logged with its
/// first run; 15 minutes at most.
async fn run_setup(project: &Project, c: &Checkout, g: &Global) -> Result<()> {
    let Some(cmd) = &project.settings.setup else {
        return Ok(());
    };
    let id = c.run_id("setup");
    let log_path = log_path(&id);
    std::fs::create_dir_all(log_path.parent().unwrap())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    info!("{id}: running `{cmd}` (mix.exs or mix.lock changed)");
    let status = command(project, cmd, &c.path, c.env(g))?
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .status();
    let status = tokio::time::timeout(Duration::from_secs(900), status)
        .await
        .map_err(|_| anyhow!("{id}: `{cmd}` timed out"))??;
    if !status.success() {
        bail!(
            "`{cmd}` failed in {id} ({status}); see {}",
            log_path.display()
        );
    }
    info!("{id}: done");
    Ok(())
}

/// Kill a running service's group and start it again.
async fn respawn(id: &str, p: &mut Proc) {
    kill_group(&p.child);
    let _ = p.child.wait().await;
    match spawn(&p.project, &p.checkout, &p.name, &p.global, true) {
        Ok(child) => {
            p.child = child;
            p.started = Instant::now();
            p.failures = 0;
            p.respawn_at = None;
            if let Some((dir, files)) = &mut p.mix {
                *files = mix_files(dir);
            }
            info!("{id}: restarted");
        }
        Err(e) => warn!("{e:#}"),
    }
}

impl Servers {
    /// Watch the project dirs of the Mix services in `procs`, and only those.
    fn sync_watches(&self, procs: &HashMap<String, Proc>) {
        let want: HashSet<PathBuf> = procs
            .values()
            .filter_map(|p| p.mix.as_ref())
            .flat_map(|(dir, _)| [dir.clone(), dir.join("config")])
            .filter(|d| d.is_dir())
            .collect();
        let mut watched = self.watched.lock().unwrap();
        if *watched == want {
            return;
        }
        let mut watcher = self.watcher.lock().unwrap();
        if watcher.is_none() {
            let changed = self.changed.clone();
            let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(ev) = res else { return };
                let mut changed = changed.lock().unwrap();
                for dir in ev.paths.iter().filter_map(|p| mix_dir(p)) {
                    changed.insert(dir, Instant::now());
                }
            });
            match w {
                Ok(w) => *watcher = Some(w),
                Err(e) => {
                    warn!("watching Mix projects: {e}");
                    return;
                }
            }
        }
        let w = watcher.as_mut().unwrap();
        for d in watched.difference(&want) {
            let _ = w.unwatch(d);
        }
        for d in want.difference(&watched) {
            if let Err(e) = w.watch(d, RecursiveMode::NonRecursive) {
                warn!("watching {}: {e}", d.display());
            }
        }
        *watched = want;
    }

    /// Restart running Mix services whose project files changed (and have been quiet
    /// for a second). When `mix.exs` or `mix.lock` did, the project's setup command
    /// (`mix deps.get`) runs first, once per checkout, without holding up the others.
    async fn restart_changed_mix(&self) {
        let dirs: Vec<PathBuf> = {
            let mut changed = self.changed.lock().unwrap();
            let quiet: Vec<PathBuf> = changed
                .iter()
                .filter(|(_, at)| at.elapsed() >= Duration::from_secs(1))
                .map(|(d, _)| d.clone())
                .collect();
            for d in &quiet {
                changed.remove(d);
            }
            quiet
        };
        if dirs.is_empty() {
            return;
        }
        let mut restart = Vec::new();
        let mut setups: Vec<(Project, Checkout, Global)> = Vec::new();
        for (id, p) in self.procs.lock().await.iter_mut() {
            let Some((dir, files)) = &p.mix else { continue };
            // FSEvents reports canonical paths, the dir is canonicalized too.
            if !dirs.iter().any(|d| d == dir) || p.respawn_at.is_some() {
                continue;
            }
            let now = mix_files(dir);
            if now == *files || !matches!(p.child.try_wait(), Ok(None)) {
                continue;
            }
            let what: Vec<&Path> = now
                .keys()
                .chain(files.keys())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|f| now.get(*f) != files.get(*f))
                .map(|f| f.strip_prefix(dir).unwrap_or(f))
                .collect();
            let deps = what.iter().any(|f| f.parent() == Some(Path::new("")));
            if deps
                && p.project.settings.setup.is_some()
                && !setups.iter().any(|(_, c, _)| c.path == p.checkout.path)
            {
                setups.push((p.project.clone(), p.checkout.clone(), p.global.clone()));
            }
            let what: Vec<String> = what.iter().map(|f| f.display().to_string()).collect();
            info!("{id}: {} changed; restarting", what.join(", "));
            restart.push(id.clone());
        }
        for (project, c, g) in setups {
            if let Err(e) = run_setup(&project, &c, &g).await {
                // Restarted anyway: the dependencies may be there already.
                warn!("{e:#}");
            }
        }
        let mut procs = self.procs.lock().await;
        for id in restart {
            if let Some(p) = procs.get_mut(&id)
                && p.respawn_at.is_none()
                && matches!(p.child.try_wait(), Ok(None))
            {
                respawn(&id, p).await;
            }
        }
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
                procs.insert(id.clone(), new_proc(project, c, name, g, child));
                self.sync_watches(&procs);
            }
        }
        if !svc.http {
            return Ok(());
        }
        for _ in 0..1800 {
            if listening(port).await {
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
        bail!("{id} did not listen on port {port} within 3 minutes")
    }

    /// Apply `restart` policies: called every second. An exited service with
    /// `on-failure` (non-zero exit) or `always` is started again after a backoff that
    /// doubles per quick exit (1 s .. 30 s) and resets once it stayed up a minute.
    /// Mix services whose project files changed are restarted.
    pub async fn supervise(&self) {
        self.restart_changed_mix().await;
        let mut procs = self.procs.lock().await;
        let now = Instant::now();
        let mut gone = Vec::new();
        for (id, p) in procs.iter_mut() {
            if let Some(at) = p.respawn_at {
                if now < at {
                    continue;
                }
                match spawn(&p.project, &p.checkout, &p.name, &p.global, true) {
                    Ok(child) => {
                        p.child = child;
                        p.started = now;
                        p.respawn_at = None;
                        if let Some((dir, files)) = &mut p.mix {
                            *files = mix_files(dir);
                        }
                    }
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

    /// Kill and start again a running service (after a pull).
    pub async fn restart(&self, id: &str) {
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
    fn mix_commands() {
        assert!(is_mix("mix phx.server"));
        assert!(is_mix("iex -S mix phx.server"));
        assert!(is_mix("with-secrets -- /nix/store/x/bin/mix run --no-halt"));
        assert!(!is_mix("bun run dev"));
        assert!(!is_mix("mixer serve"));

        let services: Services = r#"{"web": {"exec": "mix phx.server"},
                                     "api": {"exec": "mix phx.server", "restartOnMixChange": false}}"#
            .parse()
            .unwrap();
        assert!(services.0["web"].restart_on_mix_change);
        assert!(!services.0["api"].restart_on_mix_change);
    }

    #[test]
    fn mix_project_files() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
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
        let before = mix_files(dir);
        assert_eq!(before.len(), 3);
        // Rewritten with the same content: nothing to restart for.
        std::fs::write(dir.join("mix.lock"), "a").unwrap();
        assert_eq!(mix_files(dir), before);
        std::fs::write(dir.join("mix.lock"), "b").unwrap();
        assert_ne!(mix_files(dir), before);

        assert_eq!(mix_dir(&dir.join("mix.lock")).as_deref(), Some(dir));
        assert_eq!(
            mix_dir(&dir.join("config/runtime.exs")).as_deref(),
            Some(dir)
        );
        assert_eq!(mix_dir(&dir.join("config")).as_deref(), Some(dir));
        assert_eq!(mix_dir(&dir.join("lib/a.exs")), None);
        assert_eq!(mix_dir(&dir.join("README.md")), None);
    }
}
