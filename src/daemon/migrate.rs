//! Migrations and syncing: the primary's (then the template), worktrees', pulls.

use super::*;

impl Daemon {
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
    /// Setup runs first (once, `setup_primary`); while its failure backs off, nothing
    /// runs unless `force`.
    ///
    /// Holds the primary's migrate lock (its services wait on it), not `rt.lock`.
    pub(super) async fn migrate(&self, rt: &ProjectRt, force: bool) -> Result<()> {
        let primary = rt.primary();
        let lock = rt.migrate_lock(&primary);
        let result = {
            let _g = lock.lock().await;
            let _hold = self.servers.hold(&primary.path);
            match primary_setup_step(
                rt.setup_pending(&primary),
                rt.migrate_failure(&primary).as_ref(),
                force,
            ) {
                SetupStep::BackingOff => return Ok(()),
                SetupStep::Run => self.setup_primary(rt, &primary).await?,
                SetupStep::Done => {}
            }
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
    pub(super) async fn migrate_primary(&self, rt: &ProjectRt, primary: &Checkout) -> Result<bool> {
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
            Some(h) if !fresh && *rt.last_migrated.lock() == Some(h) => {
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
                &primary.path,
                &primary.run_id("seed"),
                cmd,
                &primary.path,
                primary.env(&self.global),
            )
            .await?;
        }
        if let Some(head) = head {
            // LAZY_COW_TREE_POSTGRES_TEMPLATE_REFRESH=manual: `lazy-cow-tree snapshot` only.
            if !rt.project.template_refresh_manual() {
                for kind in primary.db_kinds() {
                    let (dev, template) = (primary.dev_db_of(kind), primary.template_db_of(kind));
                    if !self.pg.exists(&dev).await? {
                        continue;
                    }
                    let t = std::time::Instant::now();
                    self.pg.snapshot(&dev, &template, &self.create_lock).await?;
                    info!("{template} refreshed from {dev} in {:?}", t.elapsed());
                }
            }
            // Not at startup: only when the base branch moved while we watched.
            let pulled = rt.last_migrated.lock().replace(head).is_some();
            if pulled {
                self.restart_on_pull(primary).await;
            }
        }
        Ok(true)
    }

    /// The project's migrate command (with the default service's env) and each
    /// service's own (with its env, in its cwd), in a checkout.
    pub(super) async fn run_migrations(&self, rt: &ProjectRt, c: &Checkout) -> Result<()> {
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
                .run_command(rt, &c.path, &format!("{id}+migrate"), &cmd, &cwd, env)
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
    pub(super) async fn run_command(
        &self,
        rt: &ProjectRt,
        checkout: &Path,
        id: &str,
        cmd: &str,
        cwd: &Path,
        env: Vec<(String, String)>,
    ) -> Result<()> {
        crate::server::run_logged(&rt.project, checkout, id, cmd, cwd, env, false).await
    }

    /// Pull branches and merge the base branch into worktrees, then migrate the
    /// worktrees that moved (the primary goes through `migrate`).
    pub(super) async fn sync(&self, rt: &ProjectRt) -> Result<()> {
        let has_remote = Repository::open(&rt.project.root)?
            .find_remote(&rt.project.settings.remote)
            .is_ok();
        if rt.project.settings.no_sync || !has_remote {
            return Ok(());
        }
        // Merged files restart services once their checkouts are migrated.
        let _hold = self.servers.hold(&rt.project.root);
        let moved = {
            let _g = rt.lock.lock().await;
            let syncer = rt.syncer();
            tokio::task::spawn_blocking(move || syncer.sync()).await??
        };
        // Outside `rt.lock`: migrations may take minutes.
        let known: Vec<Checkout> = rt.known.lock().values().cloned().collect();
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
    pub(super) async fn restart_on_pull(&self, c: &Checkout) {
        for (name, s) in &c.services.0 {
            let id = c.service_id(name);
            if s.restart_on_pull && self.servers.running(&id).await {
                self.servers.restart(&id).await;
            }
        }
    }
}
