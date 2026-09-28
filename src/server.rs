//! Checkout services (`localforest.services`): each runs its command with the service's
//! env (PORT, hostname, database, Redis) in its own process group. An http service is
//! started by the first request to its https://…localhost hostname that finds nothing
//! listening (or `localforest service start`), after the services it depends on; all of
//! them are killed (whole group, SIGKILL) with the checkout. `restart` restarts one
//! that exits on its own; `restartOnPull` one whose checkout pulled the base branch.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
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
}

impl Servers {
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
                procs.insert(
                    id.clone(),
                    Proc {
                        child,
                        project: project.clone(),
                        checkout: c.clone(),
                        name: name.to_string(),
                        global: g.clone(),
                        started: Instant::now(),
                        failures: 0,
                        respawn_at: None,
                    },
                );
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
    pub async fn supervise(&self) {
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
    }

    /// Kill and start again a running service (after a pull).
    pub async fn restart(&self, id: &str) {
        let mut procs = self.procs.lock().await;
        let Some(p) = procs.get_mut(id) else { return };
        kill_group(&p.child);
        let _ = p.child.wait().await;
        match spawn(&p.project, &p.checkout, &p.name, &p.global, true) {
            Ok(child) => {
                p.child = child;
                p.started = Instant::now();
                p.failures = 0;
                p.respawn_at = None;
                info!("{id}: restarted");
            }
            Err(e) => warn!("{e:#}"),
        }
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
}
