//! Checkouts' roles, databases and setup: provisioning, cloning the dev database from the template, adopting and dropping what a checkout owns.

use super::*;

impl Daemon {
    /// Role, routes and Redis password; for the primary also its databases. A
    /// worktree's database is created when first connected to (`resolve_pg`).
    /// Worktrees also get their environment in `.env`. Ok(true) when the primary's
    /// dev database was just created (to be seeded).
    pub(super) async fn provision(&self, c: &Checkout) -> Result<bool> {
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
        for (svc, host, port) in c.routes() {
            let service = svc.map(|s| c.service_id(&s));
            self.routes.set(host, port, c.worktree.is_some(), service);
        }
        self.redis.allow(&c.id(), &self.redis_key_of(c));
        Ok(created)
    }

    /// Upgrade: a worktree whose name changed when names started coming from git
    /// admin dirs keeps its databases (renamed; its old role: `migrate_role`, via
    /// `legacy_id`). Skipped for anything another checkout (`others`) uses under that
    /// name.
    pub(super) async fn adopt_legacy(&self, c: &Checkout, others: &[Checkout]) -> Result<()> {
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
        Ok(())
    }

    /// Upgrade from `<project>-<worktree>` role names: the worktree's old role becomes
    /// its new one (renamed: it keeps what it owns). When the old name is ambiguous
    /// (another checkout's id, or another's old id: the collision this naming fixes),
    /// the old role stays and the new one becomes a member of it instead, so it may
    /// still use the objects the old one owns.
    pub(super) async fn migrate_role(&self, c: &Checkout) -> Result<()> {
        let Some(old) = c.legacy_id() else {
            return Ok(());
        };
        if !self.pg.role_exists(&old).await? {
            return Ok(());
        }
        let projects: Vec<Project> = self
            .projects
            .lock()
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
    /// Run the setup command once in a new worktree, after `provision` (its role and
    /// `.env` exist, so setup may use the database). A failure is only logged.
    pub(super) async fn run_setup(&self, rt: &ProjectRt, c: &Checkout) {
        if let Err(e) = self.setup_once(rt, c).await {
            warn!("{e:#}");
        }
    }

    /// The setup command in `c` unless its marker says it already succeeded there.
    pub(super) async fn setup_once(&self, rt: &ProjectRt, c: &Checkout) -> Result<()> {
        let Some(cmd) = rt.project.settings.setup.clone() else {
            return Ok(());
        };
        let marker = setup_marker(&c.path)?;
        if marker.exists() {
            return Ok(());
        }
        let id = c.run_id("setup");
        self.run_command(rt, &c.path, &id, &cmd, &c.path, c.env(&self.global))
            .await
            .context("setup failed")?;
        if let Err(e) = std::fs::write(&marker, config::now().to_string()) {
            warn!("{}: {}: {e}", c.id(), marker.display());
        }
        Ok(())
    }

    /// Setup in the primary checkout, which may predate lazy-cow-tree or its `deps/`
    /// (a fresh clone): run before its first migrate, seed or service start. Needs
    /// its migrate lock. A failure is recorded like a failed migration (status, 502
    /// page) and retried after the same backoff.
    pub(super) async fn setup_primary(&self, rt: &ProjectRt, c: &Checkout) -> Result<()> {
        let result = self.setup_once(rt, c).await;
        rt.migrated(c, result.as_ref().err().map(|e| format!("{e:#}")));
        result
    }

    /// Create a worktree's dev database if missing: a copy-on-write clone of the
    /// template, or of the primary's while there's no template and it's idle.
    /// Ok(true) when it was created.
    pub(super) async fn ensure_dev_db(&self, rt: &ProjectRt, c: &Checkout) -> Result<bool> {
        let dev = c.dev_db();
        // This database's connects wait until it's cloned and adopted; others only
        // for the clone itself (create_lock), not for `adopt`'s object locks.
        let lock = self
            .dev_locks
            .lock()
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
        // LAZY_COW_TREE_POSTGRES_COW=0: empty, then migrated and seeded like a new
        // primary database.
        let source = if !rt.project.copy_on_write() {
            None
        } else if self.pg.exists(&template).await? {
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
            None if !rt.project.copy_on_write() => {
                info!("{dev}: created empty (copy-on-write off)")
            }
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
    pub(super) async fn migrate_worktree(
        &self,
        rt: &ProjectRt,
        c: &Checkout,
        force: bool,
    ) -> Result<()> {
        if !rt.has_migrations(c) {
            return Ok(());
        }
        let lock = rt.migrate_lock(c);
        let _g = lock.lock().await;
        // Its changed files restart its services after migrating, not during.
        let _hold = self.servers.hold(&c.path);
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
            .map(|r| r.path().join("lazy-cow-tree-migrated"))
            .ok();
        if !force
            && marker
                .as_ref()
                .and_then(|m| std::fs::read_to_string(m).ok())
                .is_some_and(|m| m.trim() == oid)
        {
            return Ok(());
        }
        let mut result = self.run_migrations(rt, c).await;
        // Without copy-on-write a worktree's database starts empty, like a new
        // primary one: seeded after its first migrations (its marker names another
        // database, or none).
        if result.is_ok()
            && !force
            && !rt.project.copy_on_write()
            && let Some(cmd) = rt.project.settings.seed.clone()
        {
            result = self
                .run_command(
                    rt,
                    &c.path,
                    &c.run_id("seed"),
                    &cmd,
                    &c.path,
                    c.env(&self.global),
                )
                .await;
        }
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
    pub(super) async fn migrate_worktrees(&self, rt: &ProjectRt) {
        if rt.seed_pending.load(Ordering::SeqCst) {
            return;
        }
        let known: Vec<Checkout> = rt.known.lock().values().cloned().collect();
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
    pub(super) async fn adopt_dev_db(&self, c: &Checkout) -> Result<()> {
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
    /// with everything in them. Then `adopt_dev_db`, if its dev database exists.
    pub(super) async fn adopt_databases(&self, c: &Checkout) -> Result<()> {
        let others = self.all_checkouts();
        let dbs = self.pg.databases().await?;
        for db in &dbs {
            if !c.owns_db(db)
                || !config::db_owner(db, others.iter().chain([c])).is_some_and(|o| o.same(c))
            {
                continue;
            }
            let Some(old) = self.pg.owner(db).await? else {
                continue;
            };
            if old == c.id() || old == "postgres" || others.iter().any(|o| o.id() == old) {
                continue;
            }
            self.pg.set_owner(db, &c.id()).await?;
            self.pg.adopt(db, &old, &c.id()).await?;
            info!("{db}: handed from {old} to {}", c.id());
        }
        // A new worktree has no dev database yet: `ensure_dev_db` adopts it once cloned.
        if dbs.contains(&c.dev_db()) {
            self.adopt_dev_db(c).await?;
        }
        Ok(())
    }

    /// PostgreSQL proxy, before the connection is handed to the server (which checks
    /// the password): a checkout's role may open its own databases (its dev database is
    /// created on the spot) and the maintenance ones, unless another checkout's role
    /// made it. Every other user is refused.
    pub(super) async fn resolve_pg(&self, user: &str, db: &str) -> Result<()> {
        let projects: Vec<Arc<ProjectRt>> = self.projects.lock().values().cloned().collect();
        let found = projects.into_iter().find_map(|rt| {
            let primary = rt.primary();
            let c = if primary.id() == user {
                Some(primary)
            } else {
                rt.known.lock().values().find(|c| c.id() == user).cloned()
            };
            c.map(|c| (rt, c))
        });
        let others = self.all_checkouts();
        let create_dev = pg_access(user, found.as_ref().map(|(_, c)| c), db, &others)?;
        if let Some((rt, c)) = found {
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
    pub(super) fn all_checkouts(&self) -> Vec<Checkout> {
        self.checkouts().into_iter().map(|(_, o)| o).collect()
    }

    /// Kill the checkout's services and redis-server, drop its routes, databases and role.
    pub(super) async fn deprovision(&self, c: &Checkout) -> Result<()> {
        for (_, host, _) in c.routes() {
            self.routes.remove(&host);
        }
        self.servers.stop_checkout(c).await;
        self.redis.remove(&c.id()).await;
        for rt in self.projects.lock().values() {
            rt.migrate_failures.lock().remove(&c.id());
            rt.migrating.lock().remove(&c.id());
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
}
