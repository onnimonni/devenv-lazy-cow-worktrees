//! Ports, services started on demand or at `up`, idle stops, and the status the CLI shows.

use super::*;

impl Daemon {
    /// Record the ports of new worktrees of every registered project
    /// (`worktree::assign_ports`); every worktree's port by path.
    pub(super) async fn assign_ports(&self) -> Result<HashMap<PathBuf, u16>> {
        let projects: Vec<Project> = self
            .projects
            .lock()
            .values()
            .map(|rt| rt.project.clone())
            .collect();
        let ports =
            tokio::task::spawn_blocking(move || worktree::assign_ports(&projects)).await??;
        Ok(ports.into_iter().collect())
    }

    /// Every checkout (primary and worktrees) of every project.
    pub(super) fn checkouts(&self) -> Vec<(Arc<ProjectRt>, Checkout)> {
        let projects: Vec<Arc<ProjectRt>> = self.projects.lock().values().cloned().collect();
        let mut out = Vec::new();
        for rt in projects {
            out.push((rt.clone(), rt.primary()));
            let known: Vec<Checkout> = rt.known.lock().values().cloned().collect();
            out.extend(known.into_iter().map(|c| (rt.clone(), c)));
        }
        out
    }

    /// Start the service serving `host` (exactly, else the closest parent host; a
    /// secondary port's host starts its owner) and its dependencies if nothing listens
    /// there.
    pub(super) async fn ensure_server(&self, host: &str) -> Result<()> {
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
            Some((_, rt, c, svc, port)) => {
                if c.service(&svc)
                    .is_some_and(|s| s.start == StartMode::Manual)
                {
                    anyhow::bail!(
                        "{svc} starts manually: `lazy-cow-tree service start {svc}` in {}",
                        c.path.display()
                    );
                }
                self.ensure_service(&rt, &c, &svc, Some(port)).await
            }
            None => Ok(()),
        }
    }

    /// The redis-server key of a checkout of a registered project (its own id when
    /// the project is gone).
    pub(super) fn redis_key_of(&self, c: &Checkout) -> String {
        self.projects
            .lock()
            .values()
            .find(|rt| rt.project.name == c.project)
            .map_or_else(|| c.id(), |rt| redis_key(&rt.project, c))
    }

    /// Queue a checkout's `start = "up"` services (if any).
    pub(super) fn queue_up(&self, rt: &ProjectRt, c: Checkout) {
        if rt.project.redis_start_up() {
            let redis = self.redis.clone();
            let id = redis_key(&rt.project, &c);
            tokio::spawn(async move {
                if let Err(e) = redis.start(&id).await {
                    warn!("{e:#}");
                }
            });
        }
        if c.services.0.values().any(|s| s.start == StartMode::Up) {
            self.up_queue.lock().push((rt.project.root.clone(), c));
        }
    }

    /// Start the queued checkouts' `start = "up"` services, each in the background
    /// (a worktree's migrate first, as for a request).
    pub(super) fn start_up_queued(self: &Arc<Self>) {
        let queued: Vec<(PathBuf, Checkout)> = std::mem::take(&mut *self.up_queue.lock());
        for (root, c) in queued {
            let Some(rt) = self.projects.lock().get(&root).cloned() else {
                continue;
            };
            for (name, s) in &c.services.0 {
                if s.start != StartMode::Up {
                    continue;
                }
                let (d, rt, c, name) = (self.clone(), rt.clone(), c.clone(), name.clone());
                tokio::spawn(async move {
                    if let Err(e) = d.ensure_service(&rt, &c, &name, None).await {
                        warn!("{}: {e:#}", c.service_id(&name));
                    }
                });
            }
        }
    }

    /// Stop services idle past their `idleTimeout`: no open connections through the
    /// proxy since then. Not a failure: no restart policy applies, and the next
    /// request starts them again.
    pub(super) async fn stop_idle(&self) {
        for (id, _, _, started, timeout) in self.servers.idle_candidates().await {
            if self
                .routes
                .idle_since(&id, started)
                .is_some_and(|t| t.elapsed() >= timeout)
            {
                info!("{id}: idle for {} s; stopping", timeout.as_secs());
                self.servers.stop(&id).await;
            }
        }
    }

    /// Start a service (and what it depends on) once its checkout's database is
    /// migrated: a worktree's is migrated first if it isn't yet (an error if that
    /// fails); the primary's services wait for a migration in progress.
    pub(super) async fn ensure_service(
        &self,
        rt: &ProjectRt,
        c: &Checkout,
        svc: &str,
        port: Option<u16>,
    ) -> Result<()> {
        if c.worktree.is_some() {
            self.migrate_worktree(rt, c, false)
                .await
                .context("not starting its services")?;
        } else {
            let lock = rt.migrate_lock(c);
            let _g = lock.lock().await;
            let failure = rt.migrate_failure(c);
            match primary_setup_step(rt.setup_pending(c), failure.as_ref(), false) {
                SetupStep::BackingOff => {
                    let error = failure.map(|f| f.error).unwrap_or_default();
                    anyhow::bail!("not starting its services: {error}\n(retried later)");
                }
                SetupStep::Run => self
                    .setup_primary(rt, c)
                    .await
                    .context("not starting its services")?,
                SetupStep::Done => {}
            }
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
    pub(super) fn service_named(
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
                    anyhow!("no services configured (lazy-cow-tree.services in devenv.nix)")
                })?
                .to_string(),
        };
        if c.service(&svc).is_none() {
            anyhow::bail!("no service {svc}");
        }
        Ok((rt, c, svc))
    }

    pub(super) async fn status(&self) -> Result<Status> {
        let projects: Vec<Arc<ProjectRt>> = self.projects.lock().values().cloned().collect();
        let dbs = self.pg.sizes().await.unwrap_or_default();
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
                        stopped: self.servers.stopped(&c.service_id(name)),
                    });
                }
                statuses.push(CheckoutStatus {
                    url: format!("https://{}", c.main_host()),
                    databases: dbs
                        .iter()
                        .filter(|(d, _)| c.owns_db(d))
                        .map(|(d, _)| d.clone())
                        .collect(),
                    database_bytes: dbs
                        .iter()
                        .filter(|(d, _)| c.owns_db(d))
                        .map(|(_, b)| *b as u64)
                        .sum(),
                    services,
                    redis: self.redis.running(&redis_key(&rt.project, &c)),
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
            version: env!("CARGO_PKG_VERSION").to_string(),
            projects: out,
            pg_port: self.global.pg_port,
            redis_port: self.global.redis_port,
            https_port: self.global.https_port,
            pg_disk: self.pg.disk(),
        })
    }
}
