//! Worktrees: reconciling with `git worktree list`, creating, removing (by hand or once their PR merged).

use super::*;

impl Daemon {
    /// Provision new worktrees, clean up vanished ones.
    pub(super) async fn reconcile(&self, rt: &ProjectRt) -> Result<()> {
        let _g = rt.lock.lock().await;
        self.reconcile_locked(rt).await
    }

    pub(super) async fn reconcile_locked(&self, rt: &ProjectRt) -> Result<()> {
        let root = rt.project.root.clone();
        let infos = tokio::task::spawn_blocking(move || worktree::list(&root)).await??;
        if self.provision(&rt.primary()).await? {
            rt.seed_pending.store(true, Ordering::SeqCst);
            rt.trigger(&rt.pending.migrate);
        }
        if !rt.up_queued.swap(true, Ordering::SeqCst) {
            self.queue_up(rt, rt.primary());
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
        let known = rt.known.lock().clone();
        for (name, c) in &current {
            // Known with the same settings: nothing to do. Carried over from an earlier
            // registration of the project (so still connectable meanwhile) but
            // changed: provisioned again.
            if known.get(name) == Some(c) || initializing.contains(name) {
                continue;
            }
            let fresh = !known.contains_key(name);
            let info = infos.iter().find(|i| &i.name == name);
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
                        if let Err(e) = self.adopt_legacy(c, &others).await {
                            warn!("worktree {name}: adopting its old databases: {e:#}");
                        }
                    }
                    info!(
                        "worktree {name}: https://{} -> 127.0.0.1:{}",
                        c.host(),
                        c.port
                    );
                    rt.known.lock().insert(name.clone(), c.clone());
                    rt.trigger(&rt.pending.migrate_worktrees);
                    self.queue_up(rt, c.clone());
                    if fresh && info.is_some() {
                        self.run_setup(rt, c).await;
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
                rt.known.lock().remove(name);
            }
        }
        Ok(())
    }
    pub(super) async fn create_worktree(&self, req: CreateReq) -> Result<CreateResp> {
        let rt = self.project(&req.root)?;
        let resp = {
            let _g = rt.lock.lock().await;
            self.create_locked(&rt, &req.name, req.base.clone()).await?
        };
        // Before its URL is handed out; a failure shows in status and on its pages.
        let c = rt.known.lock().get(&req.name).cloned();
        if let Some(c) = c
            && let Err(e) = self.migrate_worktree(&rt, &c, false).await
        {
            warn!("{}: {e:#}", c.id());
        }
        Ok(resp)
    }

    pub(super) async fn create_locked(
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
        self.provision(&c).await?;
        rt.known.lock().insert(name.to_string(), c.clone());
        self.run_setup(rt, &c).await;
        Ok(CreateResp {
            path,
            url: format!("https://{}", c.main_host()),
            env: c.env(&self.global),
        })
    }

    pub(super) async fn remove_worktree(&self, req: RemoveReq) -> Result<Vec<String>> {
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
        self.remove_locked(&rt, &info, &req.keep_pids, pr).await
    }

    /// Ok(Some(pr)) when a merged PR is what makes it safe.
    pub(super) async fn check_removable(
        &self,
        rt: &ProjectRt,
        info: &worktree::Info,
    ) -> Result<Option<u64>> {
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
    pub(super) async fn branch_disposable(&self, rt: &ProjectRt, info: &worktree::Info) -> bool {
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
    pub(super) async fn remove_locked(
        &self,
        rt: &ProjectRt,
        info: &worktree::Info,
        keep: &[i32],
        pr: Option<u64>,
    ) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
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
        worktree::kill_processes_in(&info.path, keep, false).await;
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
        // Not while its migrations run: they would clone its dev database again.
        let lock = rt.migrate_lock(&c);
        let _migrating = lock.lock().await;
        if let Err(e) = self.deprovision(&c).await {
            // Still known: the next reconcile finds it gone and tries again.
            anyhow::bail!(
                "removed {}, but not its databases yet (retrying later): {e:#}",
                info.name
            );
        }
        rt.known.lock().remove(&info.name);
        info!("removed worktree {}", info.name);
        Ok(warnings)
    }

    /// Remove every worktree whose branch's PR merged with nothing left unmerged.
    pub(super) async fn remove_merged(&self, rt: &ProjectRt) -> Result<()> {
        let Some(gh) = rt.gh.clone() else {
            return Ok(());
        };
        if rt.project.settings.no_auto_remove {
            return Ok(());
        }
        let _g = rt.lock.lock().await;
        let root = rt.project.root.clone();
        let infos = tokio::task::spawn_blocking(move || worktree::list(&root)).await??;
        let managed = rt
            .project
            .worktrees_dir()
            .canonicalize()
            .unwrap_or_default();
        for info in infos {
            let Some(branch) = info.branch.clone() else {
                continue;
            };
            if branch == rt.base || !info.path.starts_with(&managed) {
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
            if let Err(e) = self.remove_locked(rt, &info, &[], Some(number)).await {
                warn!("removing {}: {e:#}", info.name);
            }
        }
        Ok(())
    }
}
